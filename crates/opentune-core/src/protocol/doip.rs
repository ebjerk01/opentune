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
use crate::protocol::diag::{is_response_pending, DiagTransport};
use crate::{Error, Result};
use std::time::{Duration, Instant};

/// DoIP protocol version. 0x02 = ISO 13400-2:2012; the header carries its
/// bitwise inverse for validation.
pub const PROTOCOL_VERSION: u8 = 0x02;

/// The "default" protocol version (ISO 13400-2 Table 16): a tester that does
/// not know an entity's version may send its vehicle identification requests
/// (`0x0001`..=`0x0003`) with version `0xFF`. Not valid for any other message.
pub const DEFAULT_PROTOCOL_VERSION: u8 = 0xFF;

/// The fixed DoIP header length: version, inverse version, payload type (2),
/// payload length (4).
pub const HEADER_LEN: usize = 8;

/// **P2\* / extended per-response budget** for a diagnostic exchange: after the
/// ECU answers UDS NRC 0x78 (`responsePending`) the transport keeps receiving
/// under at least this window per follow-up message.
pub const P2_STAR_MS: u64 = 5000;

/// **Absolute cap on one diagnostic exchange.** Captured from a monotonic clock
/// when the request is sent, it bounds the *total* time spent waiting across all
/// interleaved acks, alive-checks and `responsePending` replies, so a chatty or
/// hostile entity cannot reset the wait indefinitely by trickling traffic. It
/// is a ceiling only: an ECU that never answers fails after the caller's
/// per-request timeout (see [`DoIpTransport`]'s `request`).
pub const TOTAL_DIAG_TIMEOUT_MS: u64 = 60_000;

/// **A_DoIP_Diagnostic_Message** (ISO 13400-2): how long the tester waits for
/// the entity's diagnostic-message acknowledgement (`0x8002` / `0x8003`). Until
/// the ack arrives the response wait is at least this long, so a short
/// per-request timeout does not fail a conformant gateway that acks slowly.
pub const DIAG_ACK_TIMEOUT_MS: u64 = 2000;

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
    /// DoIP entity status request (UDP, empty payload).
    pub const ENTITY_STATUS_REQUEST: u16 = 0x4001;
    /// DoIP entity status response: node type, socket counts, max data size.
    pub const ENTITY_STATUS_RESPONSE: u16 = 0x4002;
    /// Diagnostic power mode information request (UDP, empty payload).
    pub const POWER_MODE_REQUEST: u16 = 0x4003;
    /// Diagnostic power mode information response (1 byte).
    pub const POWER_MODE_RESPONSE: u16 = 0x4004;
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

    /// Encode as a vehicle announcement / identification response payload (32
    /// bytes, or 33 with the sync status) — the inverse of [`parse`](Self::parse).
    pub fn to_payload(&self) -> Vec<u8> {
        let mut p = Vec::with_capacity(Self::MIN_LEN + 1);
        p.extend_from_slice(&self.vin);
        p.extend_from_slice(&self.logical_address.to_be_bytes());
        p.extend_from_slice(&self.eid);
        p.extend_from_slice(&self.gid);
        p.push(self.further_action);
        if let Some(s) = self.sync_status {
            p.push(s);
        }
        p
    }

    /// The EID as colon-separated hex (it is usually a MAC address).
    pub fn eid_string(&self) -> String {
        hex_colon(&self.eid)
    }

    /// The GID as colon-separated hex.
    pub fn gid_string(&self) -> String {
        hex_colon(&self.gid)
    }

    /// The VIN as a trimmed ASCII string (trailing NULs/spaces removed).
    pub fn vin_string(&self) -> String {
        String::from_utf8_lossy(&self.vin)
            .trim_end_matches(['\0', ' '])
            .to_string()
    }
}

fn hex_colon(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Whether `version` is a protocol version an ISO 13400-2 entity accepts for
/// `payload_type`: 0x01 (2010), 0x02 (2012) and 0x03 (2019) for everything,
/// plus the default version 0xFF for vehicle identification requests only.
pub fn version_supported(version: u8, payload_type: u16) -> bool {
    matches!(version, 0x01..=0x03)
        || (version == DEFAULT_PROTOCOL_VERSION
            && matches!(
                payload_type,
                payload_type::VEHICLE_ID_REQUEST
                    | payload_type::VEHICLE_ID_REQUEST_EID
                    | payload_type::VEHICLE_ID_REQUEST_VIN
            ))
}

/// Whether `len` is a valid payload length for `ptype` (ISO 13400-2 Tables
/// 2-11 and 19-27). `None` for a payload type this crate does not know.
pub fn payload_len_valid(ptype: u16, len: usize) -> Option<bool> {
    use payload_type::*;
    Some(match ptype {
        GENERIC_NEGATIVE_ACK => len == 1,
        VEHICLE_ID_REQUEST | ALIVE_CHECK_REQUEST | ENTITY_STATUS_REQUEST | POWER_MODE_REQUEST => {
            len == 0
        }
        VEHICLE_ID_REQUEST_EID => len == 6,
        VEHICLE_ID_REQUEST_VIN => len == 17,
        VEHICLE_ANNOUNCEMENT => len == 32 || len == 33,
        ROUTING_ACTIVATION_REQUEST => len == 7 || len == 11,
        ROUTING_ACTIVATION_RESPONSE => len == 9 || len == 13,
        ALIVE_CHECK_RESPONSE => len == 2,
        ENTITY_STATUS_RESPONSE => len == 3 || len == 7,
        POWER_MODE_RESPONSE => len == 1,
        DIAGNOSTIC_MESSAGE | DIAGNOSTIC_MESSAGE_POS_ACK | DIAGNOSTIC_MESSAGE_NEG_ACK => len >= 5,
        _ => return None,
    })
}

/// The strict generic-header check an ISO 13400-2 entity runs on every
/// message (§6.6, Figure 7), in its order: a short header, an inverse-version
/// mismatch or an unsupported version is `incorrectPatternFormat` (0x00), a
/// declared payload longer than `max_payload` is `messageTooLarge` (0x02).
/// Returns `(version, payload type, payload length)`, or the generic NACK code
/// to answer with. Payload-type and payload-length checks (0x01 / 0x04) depend
/// on the transport and are left to the caller ([`payload_len_valid`]).
pub fn check_header(hdr: &[u8], max_payload: usize) -> std::result::Result<(u8, u16, usize), u8> {
    if hdr.len() < HEADER_LEN || hdr[1] != !hdr[0] {
        return Err(generic_nack::INCORRECT_PATTERN);
    }
    let version = hdr[0];
    let ptype = u16::from_be_bytes([hdr[2], hdr[3]]);
    if !version_supported(version, ptype) {
        return Err(generic_nack::INCORRECT_PATTERN);
    }
    let plen = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
    if plen > max_payload {
        return Err(generic_nack::MESSAGE_TOO_LARGE);
    }
    Ok((version, ptype, plen))
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
        self.encode_with_version(PROTOCOL_VERSION)
    }

    /// Encode with an explicit protocol version in the header (e.g.
    /// [`DEFAULT_PROTOCOL_VERSION`] for a vehicle identification request).
    pub fn encode_with_version(&self, version: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(version);
        out.push(!version);
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
    /// Whether the last successful activation returned code 0x11
    /// ("activated — confirmation required / further action needed") rather than
    /// 0x10 (plain success). Kept distinct so a caller can tell the two apart
    /// instead of treating 0x11 as an unconditional success.
    confirmation_required: bool,
    /// The DoIP entity's logical address, learned from the routing-activation
    /// response (ISO 13400-2 §7.1). May differ from `target_address` when a
    /// gateway routes to several ECUs.
    entity_address: Option<u16>,
}

/// Remaining time until `deadline`, as whole milliseconds clamped to a minimum of
/// 1 (so a read always gets a non-zero timeout); `None` once the deadline has
/// passed.
pub(crate) fn remaining_ms(deadline: Instant) -> Option<u64> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some((left.as_millis().min(u64::MAX as u128) as u64).max(1))
    }
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
            confirmation_required: false,
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
        // Bound the whole wait with one absolute deadline so that a stream of
        // alive-checks cannot reset the per-message timeout forever.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let resp = loop {
            let budget = remaining_ms(deadline).ok_or(Error::Timeout(timeout_ms))?;
            let msg = self.read_message(budget)?;
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
        // Response (ISO 13400-2 Table 22): tester logical addr (2) + entity
        // logical addr (2) + response code (1) + reserved (4) [+ optional OEM
        // (4)]. Require the fixed 9-byte minimum so the code byte and the echoed
        // tester address are really present.
        if resp.payload.len() < 9 {
            return Err(Error::Protocol(format!(
                "short routing activation response: {} bytes, need at least 9",
                resp.payload.len()
            )));
        }
        // The entity must echo our tester (source) logical address, confirming
        // the activation is correlated to this socket and not another tester's.
        let echoed_tester = u16::from_be_bytes([resp.payload[0], resp.payload[1]]);
        if echoed_tester != self.source_address {
            return Err(Error::Protocol(format!(
                "routing activation echoed tester address 0x{echoed_tester:04X} != ours 0x{:04X}",
                self.source_address
            )));
        }
        self.entity_address = Some(u16::from_be_bytes([resp.payload[2], resp.payload[3]]));
        let code = resp.payload[4];
        match code {
            routing_activation::RESPONSE_SUCCESS => {
                self.confirmation_required = false;
            }
            routing_activation::RESPONSE_SUCCESS_CONFIRMATION_REQUIRED => {
                // 0x11 is "activated, but confirmation / further action required".
                // Recorded distinctly from a plain 0x10 success so the caller is
                // not misled into treating it as unconditionally ready.
                self.confirmation_required = true;
            }
            _ => {
                return Err(Error::Protocol(format!(
                    "routing activation denied: response code 0x{code:02X} ({})",
                    routing_activation::name(code)
                )));
            }
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
        // One absolute deadline for the whole wait, so interleaved alive-checks
        // cannot extend it past `timeout_ms`.
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let budget = remaining_ms(deadline).ok_or(Error::Timeout(timeout_ms))?;
            let msg = self.read_message(budget)?;
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

    /// True once routing activation has succeeded (response code 0x10 **or**
    /// 0x11). Use [`activation_confirmation_required`](Self::activation_confirmation_required)
    /// to tell the two apart.
    pub fn is_activated(&self) -> bool {
        self.activated
    }

    /// True when routing activation succeeded with code 0x11 ("confirmation
    /// required / further action needed") rather than 0x10 (plain success). The
    /// socket is activated either way, but on 0x11 the entity may still gate
    /// diagnostics behind an out-of-band confirmation, so a caller should not
    /// treat it as an unconditional green light.
    pub fn activation_confirmation_required(&self) -> bool {
        self.confirmation_required
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

    /// Validate that a diagnostic message / ack echoes a sane source/target pair:
    /// the message must be addressed to us (target == our source address) and
    /// come from the ECU we are talking to (source == our target, or the learned
    /// entity address when a gateway relays). Rejects a reply addressed to a
    /// different tester or forged from an unrelated source so a cross-talking
    /// message on the socket is not accepted as this exchange's answer.
    fn correlate_addresses(&self, kind: &str, payload: &[u8]) -> Result<()> {
        // source(2) + target(2) are the first four bytes of every diagnostic /
        // ack payload.
        let src = u16::from_be_bytes([payload[0], payload[1]]);
        let tgt = u16::from_be_bytes([payload[2], payload[3]]);
        if tgt != self.source_address {
            return Err(Error::Protocol(format!(
                "DoIP {kind} target address 0x{tgt:04X} is not our tester address 0x{:04X}",
                self.source_address
            )));
        }
        let expected = self.entity_address.unwrap_or(self.target_address);
        if src != self.target_address && src != expected {
            return Err(Error::Protocol(format!(
                "DoIP {kind} source address 0x{src:04X} is neither the target 0x{:04X} nor the entity 0x{expected:04X}",
                self.target_address
            )));
        }
        Ok(())
    }

    /// Read DoIP messages until a diagnostic message (the UDS response) arrives,
    /// validating and skipping any interleaved diagnostic-message acks. Reads
    /// run under `wait`'s current deadline; a positive ack moves that deadline
    /// to one response window after the ack (never past the hard cap), while
    /// alive-checks do not move it.
    fn read_uds_response(&mut self, wait: &mut ResponseWait) -> Result<Vec<u8>> {
        loop {
            let budget = remaining_ms(wait.deadline).ok_or(Error::Timeout(wait.window_ms))?;
            let msg = match self.read_message(budget) {
                Err(Error::Timeout(_)) => return Err(Error::Timeout(wait.window_ms)),
                other => other?,
            };
            match msg.payload_type {
                payload_type::DIAGNOSTIC_MESSAGE => {
                    // source(2) + target(2) + UDS data.
                    if msg.payload.len() < 4 {
                        return Err(Error::Protocol("short DoIP diagnostic message".into()));
                    }
                    self.correlate_addresses("diagnostic message", &msg.payload)?;
                    return Ok(msg.payload[4..].to_vec());
                }
                payload_type::DIAGNOSTIC_MESSAGE_POS_ACK => {
                    // source(2) + target(2) + ack code(1): the full 5 bytes must
                    // be present. code 0 = ok; keep reading for the real response.
                    if msg.payload.len() < 5 {
                        return Err(Error::Protocol(format!(
                            "short DoIP diagnostic positive ack: {} bytes, need 5",
                            msg.payload.len()
                        )));
                    }
                    self.correlate_addresses("diagnostic positive ack", &msg.payload)?;
                    if msg.payload[4] != 0x00 {
                        return Err(Error::Protocol(format!(
                            "diagnostic message ack code 0x{:02X}",
                            msg.payload[4]
                        )));
                    }
                    // The message reached the target: P2 runs from here.
                    wait.extend(wait.response_ms);
                }
                payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK => {
                    if msg.payload.len() < 5 {
                        return Err(Error::Protocol(format!(
                            "short DoIP diagnostic negative ack: {} bytes, need 5",
                            msg.payload.len()
                        )));
                    }
                    self.correlate_addresses("diagnostic negative ack", &msg.payload)?;
                    let code = msg.payload[4];
                    return Err(Error::Protocol(format!(
                        "diagnostic message negative ack 0x{code:02X} ({})",
                        diag_nack::name(code)
                    )));
                }
                payload_type::ALIVE_CHECK_REQUEST => {
                    // The entity is probing the socket mid-exchange; answer and
                    // keep waiting for the diagnostic response under the same
                    // deadline.
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
        // Send the request exactly once. On responsePending we keep *receiving*;
        // the request is never re-sent, so a write is never duplicated.
        self.send_diagnostic(req)?;
        let mut wait = ResponseWait::new(Instant::now(), timeout_ms);
        loop {
            let resp = self.read_uds_response(&mut wait)?;
            if is_response_pending(req, &resp) {
                // responsePending for this request: the ECU extends the
                // response window to P2* (no re-send), never past the hard cap.
                wait.extend(wait.response_ms.max(P2_STAR_MS));
                continue;
            }
            return Ok(resp);
        }
    }
}

/// The deadlines of one diagnostic exchange (ISO 13400-2 + ISO 14229-2).
///
/// * Until the entity acknowledges the diagnostic message, the wait is the
///   caller's timeout but at least [`DIAG_ACK_TIMEOUT_MS`].
/// * A positive ack restarts the window at the caller's timeout (P2).
/// * A `responsePending` for this request restarts it at P2\* (at least
///   [`P2_STAR_MS`]).
/// * Nothing moves it past `hard`, [`TOTAL_DIAG_TIMEOUT_MS`] (or the caller's
///   timeout, if longer) after the request was sent; alive-checks and acks
///   never extend it beyond that.
///
/// So an ECU that never answers fails after its window, not after the cap.
#[derive(Debug, Clone, Copy)]
struct ResponseWait {
    /// The caller's per-response budget (P2 for this request).
    response_ms: u64,
    /// The window the current deadline was set from (reported on timeout).
    window_ms: u64,
    deadline: Instant,
    hard: Instant,
}

impl ResponseWait {
    fn new(sent: Instant, timeout_ms: u64) -> Self {
        let response_ms = timeout_ms.max(1);
        let hard = sent + Duration::from_millis(response_ms.max(TOTAL_DIAG_TIMEOUT_MS));
        let window_ms = response_ms.max(DIAG_ACK_TIMEOUT_MS);
        ResponseWait {
            response_ms,
            window_ms,
            deadline: (sent + Duration::from_millis(window_ms)).min(hard),
            hard,
        }
    }

    /// Restart the window at `ms` from now, capped at the hard deadline.
    fn extend(&mut self, ms: u64) {
        self.window_ms = ms;
        self.deadline = (Instant::now() + Duration::from_millis(ms)).min(self.hard);
    }
}

/// Discover DoIP entities by UDP vehicle identification (ISO 13400-2 §6.2).
///
/// Binds `bind_addr`, sends one vehicle identification request to `dest_addr`,
/// and collects every vehicle announcement that arrives within `timeout_ms`
/// (each entity answers once; a bus may hold several). Set `broadcast` to send
/// to a broadcast/multicast address. Repeated answers from one entity are
/// reported once.
///
/// A thin wrapper over [`DoIpUdpClient::identify`], which also reports
/// negative acks and rejected datagrams and takes an EID / VIN selector.
///
/// 🔒 The real-network broadcast path (`dest_addr` = `255.255.255.255:13400`,
/// `broadcast = true`) is untested against physical silicon; the offline
/// loopback/unicast path is exercised in the tests.
///
/// [`DoIpUdpClient::identify`]: crate::protocol::doip_discovery::DoIpUdpClient::identify
pub fn discover(
    bind_addr: std::net::SocketAddr,
    dest_addr: std::net::SocketAddr,
    timeout_ms: u64,
    broadcast: bool,
) -> Result<Vec<(std::net::SocketAddr, VehicleInfo)>> {
    use crate::protocol::doip_discovery::{DoIpUdpClient, VehicleIdRequest};
    let client = DoIpUdpClient::bind(bind_addr)?;
    client.set_broadcast(broadcast)?;
    let found = client.identify(dest_addr, &VehicleIdRequest::All, timeout_ms)?;
    Ok(found
        .entities
        .into_iter()
        .map(|e| (e.addr, e.info))
        .collect())
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

    /// Build a routing-activation response with an explicit echoed tester
    /// address and response code (9-byte fixed form).
    fn routing_resp(echoed_tester: u16, entity: u16, code: u8) -> DoIpMessage {
        let mut p = Vec::new();
        p.extend_from_slice(&echoed_tester.to_be_bytes());
        p.extend_from_slice(&entity.to_be_bytes());
        p.push(code);
        p.extend_from_slice(&[0u8; 4]);
        DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, p)
    }

    #[test]
    fn routing_activation_confirmation_required_is_distinct_from_success() {
        // 0x11 activates, but is recorded distinctly from a 0x10 success.
        let msg = routing_resp(
            0x0E00,
            0x1000,
            routing_activation::RESPONSE_SUCCESS_CONFIRMATION_REQUIRED,
        );
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![msg]), 0x0E00, 0x1000);
        tp.activate(1000).unwrap();
        assert!(tp.is_activated());
        assert!(
            tp.activation_confirmation_required(),
            "0x11 must set the confirmation-required flag"
        );

        // A plain 0x10 success leaves the flag clear.
        let ok = routing_resp(0x0E00, 0x1000, routing_activation::RESPONSE_SUCCESS);
        let mut tp2 = DoIpTransport::with_stream(MockStream::new(vec![ok]), 0x0E00, 0x1000);
        tp2.activate(1000).unwrap();
        assert!(tp2.is_activated());
        assert!(
            !tp2.activation_confirmation_required(),
            "0x10 must leave the confirmation-required flag clear"
        );
    }

    #[test]
    fn routing_activation_rejects_wrong_tester_echo() {
        // The entity echoes a tester address (0x0EFF) that is not ours (0x0E00).
        let msg = routing_resp(0x0EFF, 0x1000, routing_activation::RESPONSE_SUCCESS);
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![msg]), 0x0E00, 0x1000);
        assert!(matches!(tp.activate(1000), Err(Error::Protocol(_))));
        assert!(!tp.is_activated());
    }

    #[test]
    fn routing_activation_rejects_short_payload() {
        // A 5-byte response (old lenient minimum) is now rejected: the fixed
        // form carries the 4 reserved bytes, so < 9 is malformed.
        let mut p = Vec::new();
        p.extend_from_slice(&0x0E00u16.to_be_bytes());
        p.extend_from_slice(&0x1000u16.to_be_bytes());
        p.push(routing_activation::RESPONSE_SUCCESS);
        let msg = DoIpMessage::new(payload_type::ROUTING_ACTIVATION_RESPONSE, p);
        let mut tp = DoIpTransport::with_stream(MockStream::new(vec![msg]), 0x0E00, 0x1000);
        assert!(matches!(tp.activate(1000), Err(Error::Protocol(_))));
    }

    #[test]
    fn diagnostic_reply_to_wrong_tester_is_rejected() {
        let tester = 0x0E00;
        let ecu = 0x1000;
        // The ECU answers but addresses the message to a different tester 0x0E99.
        let resp_uds = vec![0x62, 0xF1, 0x90, 0x7B];
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, 0x0E99, &resp_uds),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        assert!(matches!(
            uds.read_data_by_id(0xF190),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn diagnostic_reply_from_wrong_source_is_rejected() {
        let tester = 0x0E00;
        let ecu = 0x1000;
        // A reply forged from an unrelated source address 0x2222.
        let resp_uds = vec![0x62, 0xF1, 0x90, 0x7B];
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(0x2222, tester, &resp_uds),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        assert!(matches!(
            uds.read_data_by_id(0xF190),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn short_diagnostic_positive_ack_is_rejected() {
        let tester: u16 = 0x0E00;
        let ecu: u16 = 0x1000;
        // A positive ack missing its ack-code byte (only src+tgt = 4 bytes).
        let mut p = Vec::new();
        p.extend_from_slice(&ecu.to_be_bytes());
        p.extend_from_slice(&tester.to_be_bytes());
        let short_ack = DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE_POS_ACK, p);
        let msgs = vec![routing_ok(tester, ecu), short_ack];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        assert!(matches!(
            uds.read_data_by_id(0xF190),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn diagnostic_negative_ack_is_correlated_and_named() {
        let tester: u16 = 0x0E00;
        let ecu: u16 = 0x1000;
        // A well-formed negative ack (src+tgt+code) with a known code.
        let mut p = Vec::new();
        p.extend_from_slice(&ecu.to_be_bytes());
        p.extend_from_slice(&tester.to_be_bytes());
        p.push(diag_nack::UNKNOWN_TARGET_ADDRESS);
        let nack = DoIpMessage::new(payload_type::DIAGNOSTIC_MESSAGE_NEG_ACK, p);
        let msgs = vec![routing_ok(tester, ecu), nack];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let mut uds = UdsClient::new(&mut tp);
        match uds.read_data_by_id(0xF190) {
            Err(Error::Protocol(msg)) => assert!(
                msg.contains("unknownTargetAddress"),
                "message should name the NACK code: {msg}"
            ),
            other => panic!("expected a protocol error, got {other:?}"),
        }
    }

    #[test]
    fn responsepending_does_not_resend_the_request() {
        // Two 0x78 pendings then the real response: the client sends the request
        // once and the transport keeps receiving. Assert exactly one diagnostic
        // message (the request) was written.
        let tester = 0x0E00;
        let ecu = 0x1000;
        let pending = vec![0x7F, 0x22, 0x78];
        let real = vec![0x62, 0xF1, 0x90, 0x42];
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, tester, &pending),
            diag_msg(ecu, tester, &pending),
            diag_msg(ecu, tester, &real),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        let written_before = tp.stream.written.len();
        {
            let mut uds = UdsClient::new(&mut tp);
            assert_eq!(uds.read_data_by_id(0xF190).unwrap(), vec![0x42]);
        }
        // Count DIAGNOSTIC_MESSAGE payloads written after activation: must be 1.
        let mut rest = &tp.stream.written[written_before..];
        let mut diag_requests = 0;
        while rest.len() >= HEADER_LEN {
            let (ptype, plen) = DoIpMessage::parse_header(rest).unwrap();
            if ptype == payload_type::DIAGNOSTIC_MESSAGE {
                diag_requests += 1;
            }
            rest = &rest[HEADER_LEN + plen..];
        }
        assert_eq!(
            diag_requests, 1,
            "the UDS request must be sent exactly once despite the responsePending replies"
        );
    }

    #[test]
    fn response_pending_for_another_service_is_not_waited_on() {
        // A 0x78 echoing service 0x10 is not this RDBI's pending: it is handed
        // back at once (the client reports it as mis-correlated) instead of
        // the transport waiting on it.
        let tester = 0x0E00;
        let ecu = 0x1000;
        let msgs = vec![
            routing_ok(tester, ecu),
            diag_pos_ack(ecu, tester),
            diag_msg(ecu, tester, &[0x7F, 0x10, 0x78]),
        ];
        let mut tp = DoIpTransport::with_stream(MockStream::new(msgs), tester, ecu);
        tp.activate(1000).unwrap();
        assert_eq!(
            tp.request(&[0x22, 0xF1, 0x90], 500).unwrap(),
            vec![0x7F, 0x10, 0x78]
        );
    }

    #[test]
    fn response_window_rules() {
        let t0 = Instant::now();
        let ms = |d: Instant| d.duration_since(t0).as_millis() as u64;
        // Before the ack: at least the A_DoIP_Diagnostic_Message window.
        let w = ResponseWait::new(t0, 300);
        assert_eq!(ms(w.deadline), DIAG_ACK_TIMEOUT_MS);
        assert_eq!(w.window_ms, DIAG_ACK_TIMEOUT_MS);
        assert_eq!(ms(w.hard), TOTAL_DIAG_TIMEOUT_MS);
        let w = ResponseWait::new(t0, 5000);
        assert_eq!(ms(w.deadline), 5000);
        // A caller budget above the cap raises the cap with it.
        let w = ResponseWait::new(t0, 90_000);
        assert_eq!(ms(w.hard), 90_000);
        // Extensions restart the window but never pass the cap.
        let mut w = ResponseWait::new(t0, 300);
        w.extend(300);
        assert!(ms(w.deadline) >= 300 && ms(w.deadline) < 1000);
        assert_eq!(w.window_ms, 300);
        w.hard = t0 + Duration::from_millis(100);
        w.extend(P2_STAR_MS);
        assert_eq!(ms(w.deadline), 100, "capped at the hard deadline");
    }

    /// A responsePending moves the wait to P2*: a reply that arrives after the
    /// caller's (P2) timeout but within P2* of the 0x78 is still received.
    #[test]
    fn response_pending_extends_the_window_to_p2_star() {
        use std::io::{Read, Write};
        use std::net::{TcpListener, TcpStream};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let read_msg = |sock: &mut TcpStream| -> DoIpMessage {
                let mut hdr = [0u8; HEADER_LEN];
                Read::read_exact(sock, &mut hdr).unwrap();
                let (ptype, plen) = DoIpMessage::parse_header(&hdr).unwrap();
                let mut payload = vec![0u8; plen];
                Read::read_exact(sock, &mut payload).unwrap();
                DoIpMessage::new(ptype, payload)
            };
            let req = read_msg(&mut sock);
            let tester = u16::from_be_bytes([req.payload[0], req.payload[1]]);
            Write::write_all(&mut sock, &routing_ok(tester, 0x1000).encode()).unwrap();
            read_msg(&mut sock);
            Write::write_all(&mut sock, &diag_pos_ack(0x1000, tester).encode()).unwrap();
            Write::write_all(
                &mut sock,
                &diag_msg(0x1000, tester, &[0x7F, 0x31, 0x78]).encode(),
            )
            .unwrap();
            std::thread::sleep(Duration::from_millis(700));
            Write::write_all(
                &mut sock,
                &diag_msg(0x1000, tester, &[0x71, 0x01, 0xFF, 0x00]).encode(),
            )
            .unwrap();
        });

        let stream = TcpStream::connect(addr).unwrap();
        let mut tp = DoIpTransport::with_stream(stream, 0x0E00, 0x1000);
        tp.activate(2000).unwrap();
        let t0 = Instant::now();
        let resp = tp.request(&[0x31, 0x01, 0xFF, 0x00], 300).unwrap();
        assert_eq!(resp, vec![0x71, 0x01, 0xFF, 0x00]);
        assert!(t0.elapsed() >= Duration::from_millis(650));
        server.join().unwrap();
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
