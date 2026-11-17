//! Passive full-bus capture over slcan ([`CanMonitor`]).
//!
//! Opens the channel with the LAWICEL **`L`** command (listen-only: the
//! controller never ACKs, never sends error frames, never transmits). Firmware
//! that rejects `L` (answers BEL) falls back to a normal `O` open, which still
//! never transmits a frame from OpenTune but lets the controller ACK and — at a
//! wrong bitrate — send error frames; the monitor then reports
//! [`ListenOnly::NotGuaranteed`]. Set option `listen_only=require` to refuse
//! instead of falling back.
//!
//! Options: `port` (required), `baud` (serial baud, default 115200; irrelevant
//! for USB-CDC adapters), bitrate from [`MonitorOpts::resolved_bitrate`]
//! (10k–1M presets `S0`..`S8`).
//!
//! Frames: `t`/`T` (classic), `r`/`R` (remote), and the CAN-FD extensions
//! `d`/`D` (FD) and `b`/`B` (FD + BRS) used by CANable-2 style firmware.
//! Timestamps are host arrival time (the `Z1` device timestamp is not used).
use opentune_core::monitor::MonitorClock;
use opentune_core::protocol::canfd::fd_dlc_to_len;
use opentune_core::protocol::CanFrame;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Byte stream to an slcan adapter. Reads should return within a short
/// timeout (`Ok(0)` or a `TimedOut`/`WouldBlock` error when idle).
pub trait SlcanIo: Send {
    /// Read available bytes.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize>;
    /// Write all bytes and flush.
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()>;
}

struct SerialIo(Box<dyn serialport::SerialPort>);

impl SlcanIo for SerialIo {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.write_all(buf)?;
        self.0.flush()
    }
}

/// slcan listen-only bus monitor.
#[derive(Default)]
pub struct SlcanMonitor {
    io: Option<Box<dyn SlcanIo>>,
    /// Pre-connected stream for tests, used by the next `start`.
    injected: Option<Box<dyn SlcanIo>>,
    buf: Vec<u8>,
    clock: Option<MonitorClock>,
    listen: Option<ListenOnly>,
    describe: String,
}

/// LAWICEL bitrate preset command for `bitrate`, if it is a standard rate.
pub fn bitrate_cmd(bitrate: u32) -> Option<&'static str> {
    Some(match bitrate {
        10_000 => "S0",
        20_000 => "S1",
        50_000 => "S2",
        100_000 => "S3",
        125_000 => "S4",
        250_000 => "S5",
        500_000 => "S6",
        800_000 => "S7",
        1_000_000 => "S8",
        _ => return None,
    })
}

fn is_idle(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

impl SlcanMonitor {
    /// A monitor that opens `options["port"]` on `start`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A monitor over a pre-connected stream (tests, other transports).
    pub fn with_io(io: Box<dyn SlcanIo>) -> Self {
        Self {
            injected: Some(io),
            ..Self::default()
        }
    }

    fn io(&mut self) -> Result<&mut Box<dyn SlcanIo>> {
        self.io.as_mut().ok_or(Error::NotConnected)
    }

    fn send(&mut self, cmd: &str) -> Result<()> {
        let io = self.io()?;
        io.write_all(cmd.as_bytes()).map_err(Error::Io)?;
        io.write_all(b"\r").map_err(Error::Io)
    }

    /// Wait for the command acknowledgement: `\r` = OK, BEL = error.
    /// `None` on timeout.
    fn ack(&mut self, timeout_ms: u64) -> Result<Option<bool>> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut b = [0u8; 64];
        loop {
            let n = match self.io()?.read(&mut b) {
                Ok(n) => n,
                Err(e) if is_idle(&e) => 0,
                Err(e) => return Err(Error::Io(e)),
            };
            if let Some(&c) = b[..n].iter().find(|&&c| c == b'\r' || c == 0x07) {
                return Ok(Some(c == b'\r'));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    /// Discard whatever the adapter has buffered.
    fn drain(&mut self, ms: u64) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut b = [0u8; 256];
        while Instant::now() < deadline {
            match self.io()?.read(&mut b) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if is_idle(&e) => break,
                Err(e) => return Err(Error::Io(e)),
            }
        }
        Ok(())
    }
}

/// Parse one slcan frame line (without the terminator). `None` for acks,
/// status replies and malformed lines.
pub fn parse_line(line: &[u8]) -> Option<(u32, bool, bool, CanFrame)> {
    let kind = *line.first()?;
    let (ext, rtr, fd, brs) = match kind {
        b't' => (false, false, false, false),
        b'T' => (true, false, false, false),
        b'r' => (false, true, false, false),
        b'R' => (true, true, false, false),
        b'd' => (false, false, true, false),
        b'D' => (true, false, true, false),
        b'b' => (false, false, true, true),
        b'B' => (true, false, true, true),
        _ => return None,
    };
    let s = std::str::from_utf8(&line[1..]).ok()?;
    let id_len = if ext { 8 } else { 3 };
    if s.len() < id_len + 1 {
        return None;
    }
    let id = u32::from_str_radix(&s[..id_len], 16).ok()?;
    if id > if ext { 0x1FFF_FFFF } else { 0x7FF } {
        return None;
    }
    let dlc = u8::from_str_radix(&s[id_len..id_len + 1], 16).ok()?;
    let len = if fd {
        fd_dlc_to_len(dlc)?
    } else if dlc <= 8 {
        dlc as usize
    } else {
        return None;
    };
    let hex = &s[id_len + 1..];
    let data = if rtr {
        Vec::new()
    } else {
        if hex.len() < len * 2 {
            return None;
        }
        (0..len)
            .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
            .collect::<Option<Vec<u8>>>()?
    };
    let frame = if fd {
        CanFrame::fd(id, data, brs)
    } else {
        CanFrame::classic(id, data)
    };
    Some((id, ext, rtr, frame))
}

impl CanMonitor for SlcanMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        let bitrate = opts.resolved_bitrate();
        let preset = bitrate_cmd(bitrate).ok_or_else(|| {
            Error::Config(format!(
                "slcan has no preset for {bitrate} bit/s (10k/20k/50k/100k/125k/250k/500k/800k/1M)"
            ))
        })?;
        let port_name;
        self.io = match self.injected.take() {
            Some(io) => {
                port_name = "injected".to_string();
                Some(io)
            }
            None => {
                let path = opts
                    .option("port")
                    .ok_or_else(|| Error::Config("slcan capture needs a serial port".into()))?;
                let baud = match opts.option("baud") {
                    Some(b) => b
                        .trim()
                        .parse()
                        .map_err(|_| Error::Config(format!("bad baud '{b}'")))?,
                    None => 115_200,
                };
                let port = serialport::new(path, baud)
                    .timeout(Duration::from_millis(20))
                    .open()
                    .map_err(|e| Error::Io(std::io::Error::other(e)))?;
                port_name = path.to_string();
                Some(Box::new(SerialIo(port)))
            }
        };
        self.buf.clear();
        // Close any open channel, flush stale input, set the bitrate.
        self.send("C")?;
        self.drain(100)?;
        self.send(preset)?;
        if self.ack(500)? == Some(false) {
            self.io = None;
            return Err(Error::Protocol(format!("slcan rejected bitrate {preset}")));
        }
        // Listen-only open; fall back to a normal open if unsupported.
        self.send("L")?;
        let listen = match self.ack(500)? {
            Some(true) => ListenOnly::Hardware,
            _ if opts.option("listen_only") == Some("require") => {
                let _ = self.send("C");
                self.io = None;
                return Err(Error::Unsupported(
                    "adapter refused listen-only open (L) and listen_only=require is set".into(),
                ));
            }
            _ => {
                self.send("O")?;
                if self.ack(500)? == Some(false) {
                    self.io = None;
                    return Err(Error::Protocol("slcan refused to open the channel".into()));
                }
                ListenOnly::NotGuaranteed
            }
        };
        self.listen = Some(listen);
        self.describe = format!(
            "slcan on {port_name} @ {} kbit/s, listen-only: {}",
            bitrate / 1000,
            listen.as_str()
        );
        self.clock = Some(MonitorClock::start());
        Ok(())
    }

    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        let clock = self.clock.ok_or(Error::NotConnected)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut chunk = [0u8; 512];
        loop {
            // Drain complete lines already buffered.
            while let Some(pos) = self.buf.iter().position(|&c| c == b'\r' || c == 0x07) {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                if let Some((_, extended, rtr, frame)) = parse_line(&line[..line.len() - 1]) {
                    return Ok(Some(TimedFrame {
                        ts_us: clock.now_us(),
                        extended,
                        rtr,
                        frame,
                    }));
                }
            }
            if self.buf.len() > 4096 {
                // No terminator in a long run: garbage, resynchronise.
                self.buf.clear();
            }
            let n = match self.io()?.read(&mut chunk) {
                Ok(n) => n,
                Err(e) if is_idle(&e) => 0,
                Err(e) => return Err(Error::Io(e)),
            };
            self.buf.extend_from_slice(&chunk[..n]);
            if n == 0 && Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    fn stop(&mut self) -> Result<()> {
        if self.io.is_some() {
            let _ = self.send("C");
        }
        self.io = None;
        self.clock = None;
        Ok(())
    }

    fn wall_base_us(&self) -> u64 {
        self.clock.map(|c| c.wall_base_us()).unwrap_or(0)
    }

    fn listen_only(&self) -> ListenOnly {
        self.listen.unwrap_or(ListenOnly::NotGuaranteed)
    }

    fn describe(&self) -> String {
        self.describe.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Scripted adapter: answers each command with `\r` (or BEL for the
    /// commands in `nak`), then streams `bus` bytes.
    struct FakeSlcan {
        nak: Vec<&'static str>,
        out: VecDeque<u8>,
        bus: VecDeque<u8>,
        line: Vec<u8>,
        written: Arc<Mutex<Vec<String>>>,
    }

    impl SlcanIo for FakeSlcan {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let open = self
                .written
                .lock()
                .unwrap()
                .iter()
                .any(|c| c == "L" || c == "O");
            let src = if !self.out.is_empty() {
                &mut self.out
            } else if open {
                &mut self.bus
            } else {
                return Ok(0);
            };
            let n = buf.len().min(src.len()).min(7); // small chunks: split lines
            for b in buf.iter_mut().take(n) {
                *b = src.pop_front().unwrap();
            }
            Ok(n)
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            for &c in buf {
                if c == b'\r' {
                    let cmd = String::from_utf8(std::mem::take(&mut self.line)).unwrap();
                    let reply = if self.nak.contains(&cmd.as_str()) {
                        0x07
                    } else {
                        b'\r'
                    };
                    self.out.push_back(reply);
                    self.written.lock().unwrap().push(cmd);
                } else {
                    self.line.push(c);
                }
            }
            Ok(())
        }
    }

    fn fake(nak: Vec<&'static str>, bus: &str) -> (SlcanMonitor, Arc<Mutex<Vec<String>>>) {
        let written = Arc::new(Mutex::new(Vec::new()));
        let io = FakeSlcan {
            nak,
            out: VecDeque::new(),
            bus: bus.bytes().collect(),
            line: Vec::new(),
            written: written.clone(),
        };
        (SlcanMonitor::with_io(Box::new(io)), written)
    }

    #[test]
    fn opens_listen_only_and_parses_frames() {
        let (mut m, written) = fake(
            vec![],
            "t7E8803410C1AF8000000\rT18DAF1103034100\rr1230\rz\rb12391122334455667788AABBCCDD\r",
        );
        let mut o = MonitorOpts::default();
        o.options.insert("bus".into(), "ms".into());
        m.start(&o).unwrap();
        assert_eq!(*written.lock().unwrap(), vec!["C", "S4", "L"]);
        assert_eq!(m.listen_only(), ListenOnly::Hardware);

        let f = m.next_frame(50).unwrap().unwrap();
        assert_eq!((f.id(), f.extended, f.rtr), (0x7E8, false, false));
        assert_eq!(f.data(), &[0x03, 0x41, 0x0C, 0x1A, 0xF8, 0, 0, 0]);
        let f = m.next_frame(50).unwrap().unwrap();
        assert_eq!((f.id(), f.extended), (0x18DA_F110, true));
        assert_eq!(f.data(), &[0x03, 0x41, 0x00]);
        let f = m.next_frame(50).unwrap().unwrap();
        assert!(f.rtr && f.data().is_empty());
        let f = m.next_frame(50).unwrap().unwrap();
        assert!(f.frame.fd && f.frame.brs);
        assert_eq!(f.data().len(), 12);
        assert_eq!(m.next_frame(5).unwrap(), None);
        m.stop().unwrap();
        assert_eq!(written.lock().unwrap().last().unwrap(), "C");
    }

    #[test]
    fn falls_back_to_normal_open_when_l_is_refused() {
        let (mut m, written) = fake(vec!["L"], "");
        m.start(&MonitorOpts::default()).unwrap();
        assert_eq!(*written.lock().unwrap(), vec!["C", "S6", "L", "O"]);
        assert_eq!(m.listen_only(), ListenOnly::NotGuaranteed);
    }

    #[test]
    fn require_listen_only_refuses() {
        let (mut m, _) = fake(vec!["L"], "");
        let mut o = MonitorOpts::default();
        o.options.insert("listen_only".into(), "require".into());
        assert!(matches!(m.start(&o), Err(Error::Unsupported(_))));
    }

    #[test]
    fn rejects_unknown_bitrate() {
        let (mut m, _) = fake(vec![], "");
        let o = MonitorOpts {
            bitrate: 333_000,
            ..Default::default()
        };
        assert!(matches!(m.start(&o), Err(Error::Config(_))));
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(parse_line(b"t7E").is_none());
        assert!(parse_line(b"t7E89").is_none()); // classic dlc > 8
        assert!(parse_line(b"t7E8401").is_none()); // short data
        assert!(parse_line(b"t8008").is_none()); // 11-bit id out of range
        assert!(parse_line(b"z").is_none());
        assert!(parse_line(b"t1230").is_some());
    }
}
