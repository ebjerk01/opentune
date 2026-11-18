//! The byte transport: a real serial port or anything else that moves bytes
//! (the simulator, a test double).
use std::io;
use std::time::{Duration, Instant};

/// A bidirectional byte stream with read timeouts.
pub trait Port {
    /// Send all of `data`.
    fn send(&mut self, data: &[u8]) -> io::Result<()>;
    /// Read up to `buf.len()` bytes, waiting at most `timeout` for the first
    /// one. Returns 0 on timeout.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
    /// Drop anything received but not read yet (resynchronise after an error).
    fn discard_input(&mut self) -> io::Result<()>;
}

impl<P: Port + ?Sized> Port for Box<P> {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        (**self).send(data)
    }
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        (**self).recv(buf, timeout)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        (**self).discard_input()
    }
}

/// Fill `buf` or give up at the deadline; returns how many bytes arrived.
pub fn recv_exact(port: &mut dyn Port, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
    let deadline = Instant::now() + timeout;
    let mut got = 0;
    while got < buf.len() {
        let left = deadline.saturating_duration_since(Instant::now());
        let n = port.recv(&mut buf[got..], left)?;
        if n == 0 {
            break;
        }
        got += n;
    }
    Ok(got)
}

/// Read until nothing arrives for `idle` (after a first byte within
/// `timeout`), or `max` bytes. For replies of unknown length in plain mode.
pub fn recv_until_idle(
    port: &mut dyn Port,
    max: usize,
    timeout: Duration,
    idle: Duration,
) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 64];
    let mut wait = timeout;
    while out.len() < max {
        let want = (max - out.len()).min(buf.len());
        let n = port.recv(&mut buf[..want], wait)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        wait = idle;
    }
    Ok(out)
}

/// A real serial port (8N1, no flow control).
pub struct SerialPort {
    inner: Box<dyn serialport::SerialPort>,
}

impl SerialPort {
    /// Open `path` at `baud`, then wait `settle` (boards such as the Arduino
    /// Mega behind Speeduino reset when the port opens; the INI's
    /// `delayAfterPortOpen` says how long).
    pub fn open(path: &str, baud: u32, settle: Duration) -> io::Result<Self> {
        let inner = serialport::new(path, baud)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(io::Error::other)?;
        let mut p = Self { inner };
        if !settle.is_zero() {
            std::thread::sleep(settle);
        }
        p.discard_input()?;
        Ok(p)
    }
}

impl Port for SerialPort {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        io::Write::write_all(&mut self.inner, data)?;
        io::Write::flush(&mut self.inner)
    }

    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.inner
            .set_timeout(timeout.max(Duration::from_millis(1)))
            .map_err(io::Error::other)?;
        match io::Read::read(&mut self.inner, buf) {
            Ok(n) => Ok(n),
            Err(e) if e.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(e) => Err(e),
        }
    }

    fn discard_input(&mut self) -> io::Result<()> {
        self.inner
            .clear(serialport::ClearBuffer::Input)
            .map_err(io::Error::other)
    }
}
