//! The device transport: frame a command over a serial/CDC link, read its
//! reply. Generic over any `Read + Write` so unit tests can inject a mock in
//! place of a real `serialport::SerialPort`.
use crate::proto::{self, cmd, split_status, status, status_error, Frame};
use opentune_core::{Error, Result};
use std::io::{Read, Write};

/// A single CAN frame read from the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanFrame {
    pub id: u32,
    pub extended: bool,
    pub rtr: bool,
    pub data: Vec<u8>,
}

/// Firmware identity returned by [`Device::ping`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PingInfo {
    pub proto: u8,
    pub version: (u8, u8, u8),
    pub caps: u8,
    pub id: String,
}

/// A connected OpenTune ESP32-S3 interface.
pub struct Device<S> {
    port: S,
}

impl<S: Read + Write> Device<S> {
    pub fn new(port: S) -> Self {
        Self { port }
    }

    /// Consume the device and return the underlying link.
    pub fn into_inner(self) -> S {
        self.port
    }

    // --- framing ----------------------------------------------------------

    fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.port.read_exact(buf).map_err(|_| Error::Timeout(0))
    }

    /// Read one whole device frame, validating sync and CRC.
    fn read_frame(&mut self) -> Result<Frame> {
        let mut b = [0u8; 1];
        // Resync to SYNC.
        loop {
            self.read_exact(&mut b)?;
            if b[0] == proto::SYNC {
                break;
            }
        }
        let mut hdr = [0u8; 3]; // CMD, LEN_HI, LEN_LO
        self.read_exact(&mut hdr)?;
        let len = ((hdr[1] as usize) << 8) | hdr[2] as usize;
        if len > proto::MAX_PAYLOAD {
            return Err(Error::Protocol("reply payload too large".into()));
        }
        let mut payload = vec![0u8; len];
        self.read_exact(&mut payload)?;
        let mut crc = [0u8; 1];
        self.read_exact(&mut crc)?;

        let mut body = Vec::with_capacity(3 + len);
        body.extend_from_slice(&hdr);
        body.extend_from_slice(&payload);
        if proto::crc8(&body) != crc[0] {
            return Err(Error::Protocol("reply CRC mismatch".into()));
        }
        Ok(Frame {
            cmd: hdr[0],
            payload,
        })
    }

    /// Send a command and return its reply body (status stripped), erroring on a
    /// non-OK status. Unsolicited events and stray replies are skipped.
    fn command(&mut self, cmd: u8, payload: &[u8]) -> Result<Vec<u8>> {
        let frame = proto::encode_frame(cmd, payload);
        self.port.write_all(&frame).map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)?;
        // Bounded retry so a storm of events can't wedge us forever.
        for _ in 0..64 {
            let f = self.read_frame()?;
            if !f.is_reply() {
                continue; // skip CMD_EVENT and the like
            }
            if f.base_cmd() != cmd {
                continue; // reply to a different command — ignore
            }
            let (st, body) = split_status(&f.payload)?;
            if st != status::OK {
                return Err(status_error(st));
            }
            return Ok(body.to_vec());
        }
        Err(Error::Timeout(0))
    }

    // --- commands ---------------------------------------------------------

    pub fn ping(&mut self) -> Result<PingInfo> {
        let b = self.command(cmd::PING, &[])?;
        if b.len() < 5 {
            return Err(Error::Protocol("short PING reply".into()));
        }
        Ok(PingInfo {
            proto: b[0],
            version: (b[1], b[2], b[3]),
            caps: b[4],
            id: String::from_utf8_lossy(&b[5..]).into_owned(),
        })
    }

    pub fn can_set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        self.command(cmd::CAN_SET_BITRATE, &bitrate.to_be_bytes())?;
        Ok(())
    }

    pub fn can_start(&mut self, flags: u8) -> Result<()> {
        self.command(cmd::CAN_START, &[flags])?;
        Ok(())
    }

    pub fn can_stop(&mut self) -> Result<()> {
        self.command(cmd::CAN_STOP, &[])?;
        Ok(())
    }

    pub fn can_send(&mut self, id: u32, extended: bool, rtr: bool, data: &[u8]) -> Result<()> {
        let dlc = data.len().min(8) as u8;
        let mut p = Vec::with_capacity(6 + dlc as usize);
        p.extend_from_slice(&id.to_be_bytes());
        p.push((extended as u8) | ((rtr as u8) << 1));
        p.push(dlc);
        p.extend_from_slice(&data[..dlc as usize]);
        self.command(cmd::CAN_SEND, &p)?;
        Ok(())
    }

    pub fn can_recv(&mut self, timeout_ms: u16) -> Result<CanFrame> {
        let b = self.command(cmd::CAN_RECV, &timeout_ms.to_be_bytes())?;
        if b.len() < 6 {
            return Err(Error::Protocol("short CAN_RECV reply".into()));
        }
        let id = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
        let flags = b[4];
        let dlc = b[5] as usize;
        if b.len() < 6 + dlc {
            return Err(Error::Protocol("truncated CAN_RECV data".into()));
        }
        Ok(CanFrame {
            id,
            extended: flags & 0x01 != 0,
            rtr: flags & 0x02 != 0,
            data: b[6..6 + dlc].to_vec(),
        })
    }

    /// Configure the device's on-board ISO-TP. `pad = None` disables padding.
    pub fn isotp_config(
        &mut self,
        tx_id: u32,
        rx_id: u32,
        extended: bool,
        block_size: u8,
        st_min: u8,
        pad: Option<u8>,
    ) -> Result<()> {
        let mut p = Vec::with_capacity(12);
        p.extend_from_slice(&tx_id.to_be_bytes());
        p.extend_from_slice(&rx_id.to_be_bytes());
        p.push(extended as u8);
        p.push(block_size);
        p.push(st_min);
        // 0 means "no padding"; any other value is the pad byte. If the caller
        // wants to pad with 0x00, the device treats that as no-padding — a
        // documented corner we avoid by never passing Some(0).
        p.push(pad.unwrap_or(0));
        self.command(cmd::ISOTP_CONFIG, &p)?;
        Ok(())
    }

    /// Run a UDS/KWP request over the device's on-board ISO-TP and return the
    /// reassembled response payload.
    pub fn isotp_request(&mut self, timeout_ms: u16, req: &[u8]) -> Result<Vec<u8>> {
        let mut p = Vec::with_capacity(2 + req.len());
        p.extend_from_slice(&timeout_ms.to_be_bytes());
        p.extend_from_slice(req);
        self.command(cmd::ISOTP_REQUEST, &p)
    }

    pub fn kline_set_baud(&mut self, baud: u32) -> Result<()> {
        self.command(cmd::KLINE_SET_BAUD, &baud.to_be_bytes())?;
        Ok(())
    }

    pub fn kline_init_fast(&mut self, startcomm: &[u8]) -> Result<Vec<u8>> {
        self.command(cmd::KLINE_INIT_FAST, startcomm)
    }

    pub fn kline_init_5baud(&mut self, addr: u8) -> Result<Vec<u8>> {
        self.command(cmd::KLINE_INIT_5BAUD, &[addr])
    }

    pub fn kline_transceive(&mut self, timeout_ms: u16, req: &[u8]) -> Result<Vec<u8>> {
        let mut p = Vec::with_capacity(2 + req.len());
        p.extend_from_slice(&timeout_ms.to_be_bytes());
        p.extend_from_slice(req);
        self.command(cmd::KLINE_TRANSCEIVE, &p)
    }

    pub fn battery_mv(&mut self) -> Result<u32> {
        let b = self.command(cmd::BATTERY, &[])?;
        if b.len() < 2 {
            return Err(Error::Protocol("short BATTERY reply".into()));
        }
        Ok(u16::from_be_bytes([b[0], b[1]]) as u32)
    }

    pub fn reset(&mut self) -> Result<()> {
        self.command(cmd::RESET, &[])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::encode_frame;
    use std::collections::VecDeque;

    /// A mock serial link: whatever the driver writes is captured, and reply
    /// frames queued ahead of time are handed back on read.
    #[derive(Default)]
    struct MockLink {
        written: Vec<u8>,
        to_read: VecDeque<u8>,
    }

    impl MockLink {
        fn with_reply(cmd: u8, payload: &[u8]) -> Self {
            let frame = encode_frame(cmd, payload);
            Self {
                written: Vec::new(),
                to_read: frame.into_iter().collect(),
            }
        }
        fn queue(&mut self, cmd: u8, payload: &[u8]) {
            self.to_read.extend(encode_frame(cmd, payload));
        }
    }

    impl Read for MockLink {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.to_read.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "mock empty",
                ));
            }
            let mut n = 0;
            while n < buf.len() {
                match self.to_read.pop_front() {
                    Some(b) => {
                        buf[n] = b;
                        n += 1;
                    }
                    None => break,
                }
            }
            Ok(n)
        }
    }

    impl Write for MockLink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const REPLY: u8 = proto::REPLY_BIT;

    #[test]
    fn ping_round_trip() {
        let mut body = vec![
            proto::status::OK,
            proto::PROTO_VERSION,
            0,
            1,
            0,
            proto::caps::CAN,
        ];
        body.extend_from_slice(b"OpenTune-ESP32S3");
        let link = MockLink::with_reply(cmd::PING | REPLY, &body);
        let mut dev = Device::new(link);
        let info = dev.ping().unwrap();
        assert_eq!(info.proto, proto::PROTO_VERSION);
        assert_eq!(info.version, (0, 1, 0));
        assert_eq!(info.caps, proto::caps::CAN);
        assert_eq!(info.id, "OpenTune-ESP32S3");
    }

    #[test]
    fn command_sends_correct_frame() {
        let link = MockLink::with_reply(cmd::CAN_START | REPLY, &[proto::status::OK]);
        let mut dev = Device::new(link);
        dev.can_start(0x02).unwrap();
        // The driver should have written exactly a CAN_START frame with [0x02].
        let expected = encode_frame(cmd::CAN_START, &[0x02]);
        assert_eq!(dev.into_inner().written, expected);
    }

    #[test]
    fn isotp_request_returns_body() {
        // Device replies with a UDS positive response 0x62 ....
        let resp = [proto::status::OK, 0x62, 0xF1, 0x90, b'A', b'B'];
        let link = MockLink::with_reply(cmd::ISOTP_REQUEST | REPLY, &resp);
        let mut dev = Device::new(link);
        let out = dev.isotp_request(1000, &[0x22, 0xF1, 0x90]).unwrap();
        assert_eq!(out, vec![0x62, 0xF1, 0x90, b'A', b'B']);
    }

    #[test]
    fn can_recv_decodes_frame() {
        let mut body = vec![proto::status::OK];
        body.extend_from_slice(&0x7E8u32.to_be_bytes());
        body.push(0x00); // flags
        body.push(0x03); // dlc
        body.extend_from_slice(&[0x50, 0x01, 0x02]);
        let link = MockLink::with_reply(cmd::CAN_RECV | REPLY, &body);
        let mut dev = Device::new(link);
        let f = dev.can_recv(500).unwrap();
        assert_eq!(f.id, 0x7E8);
        assert!(!f.extended);
        assert_eq!(f.data, vec![0x50, 0x01, 0x02]);
    }

    #[test]
    fn non_ok_status_is_error() {
        let link = MockLink::with_reply(cmd::CAN_SEND | REPLY, &[proto::status::ERR_CAN_DOWN]);
        let mut dev = Device::new(link);
        let err = dev.can_send(0x7E0, false, false, &[1, 2, 3]).unwrap_err();
        assert!(matches!(err, Error::Protocol(_)));
    }

    #[test]
    fn device_timeout_status_maps_to_timeout() {
        let link = MockLink::with_reply(cmd::CAN_RECV | REPLY, &[proto::status::ERR_TIMEOUT]);
        let mut dev = Device::new(link);
        assert!(matches!(dev.can_recv(100), Err(Error::Timeout(_))));
    }

    #[test]
    fn events_are_skipped_before_the_reply() {
        // An unsolicited event frame arrives before the real reply.
        let mut link = MockLink::default();
        link.queue(cmd::EVENT | REPLY, &[0xAA]); // 0xEE event, must be skipped
        link.queue(cmd::BATTERY | REPLY, &{
            let mut b = vec![proto::status::OK];
            b.extend_from_slice(&12345u16.to_be_bytes());
            b
        });
        let mut dev = Device::new(link);
        assert_eq!(dev.battery_mv().unwrap(), 12345);
    }

    #[test]
    fn stray_reply_for_other_command_is_skipped() {
        let mut link = MockLink::default();
        // A late CAN_RECV reply, then the battery reply we actually want.
        link.queue(cmd::CAN_RECV | REPLY, &[proto::status::ERR_TIMEOUT]);
        link.queue(cmd::BATTERY | REPLY, &[proto::status::OK, 0x30, 0x39]);
        let mut dev = Device::new(link);
        assert_eq!(dev.battery_mv().unwrap(), 0x3039);
    }
}
