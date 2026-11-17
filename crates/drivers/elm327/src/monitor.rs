//! Passive full-bus capture over a serial ELM327 ([`CanMonitor`]) using the
//! `ATMA` (monitor all) command.
//!
//! ## Sequence
//! `ATZ`, `ATE0`, `ATL0`, `ATS0`+`ATD0` (compact lines; `elm_spaces=1` selects
//! `ATS1`+`ATD1`), `ATH1` (show ids), `ATCAF0` (raw frames, no ISO-TP
//! formatting), `ATAL`, **`ATCSM1`** (CAN silent monitoring: the ELM does not
//! ACK), `ATPB <opts> <div>` + `ATSPB` (user CAN protocol B: options `E0` =
//! receive both 11- and 29-bit ids, variable DLC, no formatting; divisor =
//! 500 kbit / bitrate), an optional `ATCF`/`ATCM` acceptance filter, then
//! `ATMA`. Monitoring is stopped by sending one character and waiting for the
//! `>` prompt.
//!
//! ## Listen-only
//! `ATMA` itself never transmits. Whether the controller ACKs depends on
//! `ATCSM1`: genuine ELM327 ≥ v1.4b accept it ([`ListenOnly::Hardware`]); most
//! "v1.5" clones answer `?` and then ACK every frame
//! ([`ListenOnly::NotGuaranteed`]).
//!
//! ## Throughput and drops
//! The ELM prints frames as text over the serial link (38400 baud ≈ 150
//! frames/s compact). On a busy bus its buffer overflows: it prints
//! `BUFFER FULL` and stops; this monitor counts that as one **dropped** event
//! (the number of lost frames is unknown), and immediately restarts `ATMA`.
//! `<RX ERROR`/`<DATA ERROR` lines are counted as dropped too. Use an id filter
//! to keep the rate down. Timestamps are host arrival time.
//!
//! ## Options
//! `port` (default `/dev/ttyUSB0`), `baud` (default 38400), `bus` (`hs`/`ms`;
//! **MS-CAN also needs the adapter's physical HS/MS switch set to MS**),
//! `can_pb_options` / `can_pb_divisor` (override the `ATPB` bytes, hex),
//! `elm_spaces` (`1` for spaced output).
use super::ElmStream;
use opentune_core::monitor::MonitorClock;
use opentune_core::protocol::elm::at_rejected;
use opentune_core::protocol::elm::monitor::{acceptance_filter, parse_monitor_line, MonitorLine};
use opentune_core::protocol::CanFrame;
use opentune_core::{CanMonitor, Error, ListenOnly, MonitorOpts, Result, TimedFrame};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

/// Consecutive `CAN ERROR` restarts (with no frame in between) before the
/// capture gives up.
const MAX_CAN_ERRORS: u32 = 3;

struct SerialStream(Box<dyn serialport::SerialPort>);

impl ElmStream for SerialStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
    fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.0.write_all(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
    fn drain_input(&mut self) {
        let _ = self.0.clear(serialport::ClearBuffer::Input);
    }
}

/// ELM327 `ATMA` bus monitor.
#[derive(Default)]
pub struct ElmMonitor {
    stream: Option<Box<dyn ElmStream>>,
    injected: Option<Box<dyn ElmStream>>,
    buf: Vec<u8>,
    clock: Option<MonitorClock>,
    listen: Option<ListenOnly>,
    dropped: u64,
    can_errors: u32,
    monitoring: bool,
    describe: String,
}

fn is_idle(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

impl ElmMonitor {
    /// A monitor that opens the serial port on `start`.
    pub fn new() -> Self {
        Self::default()
    }

    /// A monitor over a pre-connected stream (tests).
    pub fn with_stream(stream: Box<dyn ElmStream>) -> Self {
        Self {
            injected: Some(stream),
            ..Self::default()
        }
    }

    fn stream(&mut self) -> Result<&mut Box<dyn ElmStream>> {
        self.stream.as_mut().ok_or(Error::NotConnected)
    }

    fn write(&mut self, s: &str) -> Result<()> {
        let st = self.stream()?;
        st.write_all(s.as_bytes()).map_err(Error::Io)?;
        st.flush().map_err(Error::Io)
    }

    /// Read until the `>` prompt (returned text excludes it) or the deadline.
    fn read_prompt(&mut self, timeout_ms: u64) -> Result<String> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut out = Vec::new();
        let mut b = [0u8; 256];
        loop {
            let n = match self.stream()?.read(&mut b) {
                Ok(n) => n,
                Err(e) if is_idle(&e) => 0,
                Err(e) => return Err(Error::Io(e)),
            };
            out.extend_from_slice(&b[..n]);
            if let Some(p) = out.iter().position(|&c| c == b'>') {
                out.truncate(p);
                break;
            }
            if Instant::now() >= deadline {
                if out.is_empty() {
                    return Err(Error::Timeout(timeout_ms));
                }
                break;
            }
        }
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Send an AT command, return its reply.
    fn at(&mut self, cmd: &str, timeout_ms: u64) -> Result<String> {
        self.stream()?.drain_input();
        self.write(&format!("{cmd}\r"))?;
        self.read_prompt(timeout_ms)
    }

    fn at_ok(&mut self, cmd: &str) -> Result<bool> {
        let r = self.at(cmd, 2000)?;
        Ok(!at_rejected(&r))
    }

    fn at_required(&mut self, cmd: &str) -> Result<()> {
        if self.at_ok(cmd)? {
            Ok(())
        } else {
            Err(Error::Protocol(format!("ELM327 rejected {cmd}")))
        }
    }

    fn begin_ma(&mut self) -> Result<()> {
        self.stream()?.drain_input();
        self.write("ATMA\r")?;
        self.monitoring = true;
        Ok(())
    }
}

fn opt_hex_u8(opts: &MonitorOpts, key: &str) -> Result<Option<u8>> {
    match opts.option(key) {
        None => Ok(None),
        Some(v) => {
            let t = v.trim();
            let t = t
                .strip_prefix("0x")
                .or_else(|| t.strip_prefix("0X"))
                .unwrap_or(t);
            u8::from_str_radix(t, 16)
                .map(Some)
                .map_err(|_| Error::Config(format!("{key}: bad hex byte '{v}'")))
        }
    }
}

impl CanMonitor for ElmMonitor {
    fn start(&mut self, opts: &MonitorOpts) -> Result<()> {
        let bitrate = opts.resolved_bitrate();
        let div = match opt_hex_u8(opts, "can_pb_divisor")? {
            Some(d) => d,
            None if bitrate > 0 && 500_000 % bitrate == 0 && 500_000 / bitrate <= 0x40 => {
                (500_000 / bitrate) as u8
            }
            None => {
                return Err(Error::Config(format!(
                    "ELM327 cannot monitor at {bitrate} bit/s (needs 500 kbit / n, n = 1..64)"
                )))
            }
        };
        let pb_opts = opt_hex_u8(opts, "can_pb_options")?.unwrap_or(0xE0);
        let port = opts.option("port").unwrap_or("/dev/ttyUSB0").to_string();
        self.stream = match self.injected.take() {
            Some(s) => Some(s),
            None => {
                let baud = match opts.option("baud") {
                    Some(b) => b
                        .trim()
                        .parse()
                        .map_err(|_| Error::Config(format!("bad baud '{b}'")))?,
                    None => 38_400,
                };
                let p = serialport::new(&port, baud)
                    .timeout(Duration::from_millis(30))
                    .open()
                    .map_err(|e| Error::Io(std::io::Error::other(e)))?;
                Some(Box::new(SerialStream(p)))
            }
        };
        self.buf.clear();
        self.dropped = 0;
        self.can_errors = 0;
        let spaced = opts.option("elm_spaces") == Some("1");

        let res = (|| -> Result<ListenOnly> {
            // In case a previous ATMA is still running: any char stops it.
            self.write("\r")?;
            let _ = self.read_prompt(300);
            let _ = self.at("ATZ", 3000);
            let version = self.at("ATI", 2000).unwrap_or_default();
            self.at_required("ATE0")?;
            self.at_required("ATL0")?;
            self.at_required(if spaced { "ATS1" } else { "ATS0" })?;
            let _ = self.at_ok(if spaced { "ATD1" } else { "ATD0" })?;
            self.at_required("ATH1")?;
            self.at_required("ATCAF0")?;
            let _ = self.at_ok("ATAL")?;
            let silent = self.at_ok("ATCSM1")?;
            self.at_required(&format!("ATPB {pb_opts:02X} {div:02X}"))?;
            self.at_required("ATSPB")?;
            if let Some((cf, cm, ext)) = acceptance_filter(&opts.filter_ids) {
                let (cf, cm) = if ext {
                    (format!("ATCF {cf:08X}"), format!("ATCM {cm:08X}"))
                } else {
                    (format!("ATCF {cf:03X}"), format!("ATCM {cm:03X}"))
                };
                self.at_required(&cf)?;
                self.at_required(&cm)?;
            }
            let listen = if silent {
                ListenOnly::Hardware
            } else {
                ListenOnly::NotGuaranteed
            };
            let version = if version.is_empty() {
                "ELM327".to_string()
            } else {
                version
            };
            self.describe = format!(
                "{version} on {port} @ {} kbit/s{}, listen-only: {}",
                500 / div.max(1) as u32,
                if opts.is_ms_bus() {
                    " (MS-CAN: set the adapter switch to MS)"
                } else {
                    ""
                },
                listen.as_str()
            );
            Ok(listen)
        })();
        match res {
            Ok(listen) => {
                self.listen = Some(listen);
                self.clock = Some(MonitorClock::start());
                self.begin_ma()
            }
            Err(e) => {
                self.stream = None;
                Err(e)
            }
        }
    }

    fn next_frame(&mut self, timeout_ms: u64) -> Result<Option<TimedFrame>> {
        let clock = self.clock.ok_or(Error::NotConnected)?;
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut chunk = [0u8; 512];
        loop {
            while let Some(pos) = self
                .buf
                .iter()
                .position(|&c| c == b'\r' || c == b'\n' || c == b'>')
            {
                let term = self.buf[pos];
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&line[..line.len() - 1]).to_string();
                let parsed = parse_monitor_line(&text);
                if term == b'>' {
                    // The ELM left monitor mode (buffer full, error, ...).
                    self.monitoring = false;
                }
                match parsed {
                    MonitorLine::Frame(id, extended, rtr, data) => {
                        self.can_errors = 0;
                        return Ok(Some(TimedFrame {
                            ts_us: clock.now_us(),
                            extended,
                            rtr,
                            frame: CanFrame::classic(id, data),
                        }));
                    }
                    MonitorLine::BufferFull | MonitorLine::Damaged => self.dropped += 1,
                    MonitorLine::BusError => {
                        self.can_errors += 1;
                        if self.can_errors >= MAX_CAN_ERRORS {
                            return Err(Error::Protocol(
                                "ELM327 reports CAN ERROR repeatedly: wrong bitrate, no bus, \
                                 or the HS/MS switch is in the wrong position"
                                    .into(),
                            ));
                        }
                    }
                    MonitorLine::Other => {}
                }
            }
            if !self.monitoring {
                self.begin_ma()?;
            }
            if self.buf.len() > 4096 {
                self.buf.clear();
            }
            let n = match self.stream()?.read(&mut chunk) {
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
        if self.stream.is_some() && self.monitoring {
            // Any character stops ATMA; wait for the prompt.
            let _ = self.write(" ");
            let _ = self.read_prompt(1000);
        }
        self.monitoring = false;
        self.stream = None;
        self.clock = None;
        Ok(())
    }

    fn wall_base_us(&self) -> u64 {
        self.clock.map(|c| c.wall_base_us()).unwrap_or(0)
    }

    fn dropped(&self) -> u64 {
        self.dropped
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

    /// Scripted ELM: answers AT commands with `OK>` (or `?>` for those in
    /// `refuse`), and after `ATMA` streams `bus` (which may contain its own
    /// `>` prompts, e.g. after `BUFFER FULL`). Any byte written while
    /// monitoring stops it with `STOPPED\r>`.
    struct FakeElm {
        refuse: Vec<&'static str>,
        bus: VecDeque<u8>,
        out: VecDeque<u8>,
        line: Vec<u8>,
        monitoring: bool,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl ElmStream for FakeElm {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.out.is_empty() && self.monitoring {
                // Feed the bus a few bytes at a time.
                for _ in 0..9 {
                    match self.bus.pop_front() {
                        Some(b) => {
                            if b == b'>' {
                                self.monitoring = false;
                            }
                            self.out.push_back(b);
                            if b == b'>' {
                                break;
                            }
                        }
                        None => break,
                    }
                }
            }
            let n = buf.len().min(self.out.len());
            for b in buf.iter_mut().take(n) {
                *b = self.out.pop_front().unwrap();
            }
            Ok(n)
        }
        fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
            for &c in buf {
                if self.monitoring {
                    self.monitoring = false;
                    self.out.extend(b"STOPPED\r\r>");
                    self.log.lock().unwrap().push("<stop>".into());
                    continue;
                }
                if c != b'\r' {
                    self.line.push(c);
                    continue;
                }
                let cmd = String::from_utf8(std::mem::take(&mut self.line)).unwrap();
                if cmd.is_empty() {
                    self.out.extend(b"?\r\r>");
                    continue;
                }
                self.log.lock().unwrap().push(cmd.clone());
                if cmd == "ATMA" {
                    self.monitoring = true;
                } else if cmd == "ATI" {
                    self.out.extend(b"ELM327 v1.5\r\r>");
                } else if self.refuse.contains(&cmd.as_str()) {
                    self.out.extend(b"?\r\r>");
                } else {
                    self.out.extend(b"OK\r\r>");
                }
            }
            Ok(())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn fake(refuse: Vec<&'static str>, bus: &str) -> (ElmMonitor, Arc<Mutex<Vec<String>>>) {
        let log = Arc::new(Mutex::new(Vec::new()));
        let elm = FakeElm {
            refuse,
            bus: bus.bytes().collect(),
            out: VecDeque::new(),
            line: Vec::new(),
            monitoring: false,
            log: log.clone(),
        };
        (ElmMonitor::with_stream(Box::new(elm)), log)
    }

    #[test]
    fn monitor_handshake_stream_and_buffer_full_restart() {
        let (mut m, log) = fake(
            vec![],
            "7E803410C1AF8\r280011482\rBUFFER FULL\r\r>5A0FF10\r",
        );
        let mut o = MonitorOpts::default();
        o.options.insert("bus".into(), "ms".into());
        o.filter_ids = vec![0x7E8, 0x7E0];
        m.start(&o).unwrap();
        assert_eq!(m.listen_only(), ListenOnly::Hardware);
        assert!(m.describe().contains("125 kbit/s"));
        {
            let log = log.lock().unwrap();
            for cmd in [
                "ATH1",
                "ATCAF0",
                "ATCSM1",
                "ATPB E0 04",
                "ATSPB",
                "ATCF 7E0",
                "ATCM 7F7",
                "ATMA",
            ] {
                assert!(log.iter().any(|c| c == cmd), "missing {cmd}: {log:?}");
            }
            assert_eq!(log.last().unwrap(), "ATMA");
        }
        let f = m.next_frame(50).unwrap().unwrap();
        assert_eq!(
            (f.id(), f.data()),
            (0x7E8, &[0x03, 0x41, 0x0C, 0x1A, 0xF8][..])
        );
        let f = m.next_frame(50).unwrap().unwrap();
        assert_eq!(f.id(), 0x280);
        // BUFFER FULL: counted, ATMA restarted, capture continues.
        let f = m.next_frame(50).unwrap().unwrap();
        assert_eq!((f.id(), f.data()), (0x5A0, &[0xFF, 0x10][..]));
        assert_eq!(m.dropped(), 1);
        assert_eq!(
            log.lock().unwrap().iter().filter(|c| *c == "ATMA").count(),
            2
        );
        assert_eq!(m.next_frame(5).unwrap(), None);
        m.stop().unwrap();
        assert_eq!(log.lock().unwrap().last().unwrap(), "<stop>");
    }

    #[test]
    fn clone_without_silent_mode_is_not_guaranteed() {
        let (mut m, _) = fake(vec!["ATCSM1"], "");
        m.start(&MonitorOpts::default()).unwrap();
        assert_eq!(m.listen_only(), ListenOnly::NotGuaranteed);
        assert!(m.describe().contains("500 kbit/s"));
    }

    #[test]
    fn repeated_can_error_fails() {
        let (mut m, _) = fake(vec![], "CAN ERROR\r>CAN ERROR\r>CAN ERROR\r>");
        m.start(&MonitorOpts::default()).unwrap();
        let mut err = None;
        for _ in 0..10 {
            if let Err(e) = m.next_frame(5) {
                err = Some(e);
                break;
            }
        }
        assert!(matches!(err, Some(Error::Protocol(_))));
    }

    #[test]
    fn rejects_unreachable_bitrate() {
        let (mut m, _) = fake(vec![], "");
        let o = MonitorOpts {
            bitrate: 300_000,
            ..Default::default()
        };
        assert!(matches!(m.start(&o), Err(Error::Config(_))));
    }
}
