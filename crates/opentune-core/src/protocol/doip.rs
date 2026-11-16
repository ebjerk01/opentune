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

/// DoIP payload types (ISO 13400-2, Table 17).
pub mod payload_type {
    pub const GENERIC_NEGATIVE_ACK: u16 = 0x0000;
    pub const VEHICLE_ID_REQUEST: u16 = 0x0001;
    pub const VEHICLE_ANNOUNCEMENT: u16 = 0x0004;
    pub const ROUTING_ACTIVATION_REQUEST: u16 = 0x0005;
    pub const ROUTING_ACTIVATION_RESPONSE: u16 = 0x0006;
    pub const ALIVE_CHECK_REQUEST: u16 = 0x0007;
    pub const ALIVE_CHECK_RESPONSE: u16 = 0x0008;
    pub const DIAGNOSTIC_MESSAGE: u16 = 0x8001;
    pub const DIAGNOSTIC_MESSAGE_POS_ACK: u16 = 0x8002;
    pub const DIAGNOSTIC_MESSAGE_NEG_ACK: u16 = 0x8003;
}

/// Routing-activation response codes (ISO 13400-2, Table 25). 0x10 = success.
pub mod routing_activation {
    /// Default activation type.
    pub const TYPE_DEFAULT: u8 = 0x00;
    /// "Routing successfully activated."
    pub const RESPONSE_SUCCESS: u8 = 0x10;
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

    /// Perform the routing-activation handshake (ISO 13400-2 §7.1). Must succeed
    /// before diagnostic messages are accepted by the gateway.
    pub fn activate(&mut self, timeout_ms: u64) -> Result<()> {
        // Request payload: source address (2) + activation type (1) + reserved (4).
        let mut payload = Vec::with_capacity(7);
        payload.extend_from_slice(&self.source_address.to_be_bytes());
        payload.push(routing_activation::TYPE_DEFAULT);
        payload.extend_from_slice(&[0u8; 4]); // ISO reserved
        self.write_message(&DoIpMessage::new(
            payload_type::ROUTING_ACTIVATION_REQUEST,
            payload,
        ))?;

        let resp = self.read_message(timeout_ms)?;
        if resp.payload_type != payload_type::ROUTING_ACTIVATION_RESPONSE {
            return Err(Error::Protocol(format!(
                "expected routing activation response, got payload type 0x{:04X}",
                resp.payload_type
            )));
        }
        // Response: tester addr (2) + entity addr (2) + response code (1) + reserved (4).
        if resp.payload.len() < 5 {
            return Err(Error::Protocol("short routing activation response".into()));
        }
        let code = resp.payload[4];
        if code != routing_activation::RESPONSE_SUCCESS {
            return Err(Error::Protocol(format!(
                "routing activation denied: response code 0x{code:02X}"
            )));
        }
        self.activated = true;
        Ok(())
    }

    /// True once routing activation has succeeded.
    pub fn is_activated(&self) -> bool {
        self.activated
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
                        "diagnostic message negative ack 0x{code:02X}"
                    )));
                }
                payload_type::GENERIC_NEGATIVE_ACK => {
                    let nack = msg.payload.first().copied().unwrap_or(0);
                    return Err(Error::Protocol(format!(
                        "DoIP generic negative ack 0x{nack:02X}"
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
