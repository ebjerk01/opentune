//! The byte transport: a real serial port (KKL cable / USB-TTL) or anything
//! else that moves bytes (the simulator, a test double).
use std::io;
use std::time::{Duration, Instant};

/// A bidirectional byte stream with read timeouts.
pub trait Port {
    /// Send all of `data`.
    fn send(&mut self, data: &[u8]) -> io::Result<()>;
    /// Read up to `buf.len()` bytes, waiting at most `timeout` for the first
    /// one. Returns 0 on timeout.
    fn recv(&mut self, buf: &mut [u8], timeout: Duration) -> io::Result<usize>;
    /// Drop anything received but not read yet.
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

/// Read until `n` bytes arrived or the deadline passed; returns what came.
pub fn recv_exact(port: &mut dyn Port, n: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut out = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let left = deadline.saturating_duration_since(Instant::now());
        let k = port.recv(&mut out[got..], left)?;
        if k == 0 {
            break;
        }
        got += k;
    }
    out.truncate(got);
    Ok(out)
}

/// A real serial port: 8N1, no flow control, at an explicit baud rate.
///
/// DTR and RTS are driven low after opening (the state C167BootTool puts a
/// KKL cable in before the zero byte). No BREAK is used: the BSL's autobaud
/// is the zero byte itself.
pub struct SerialPort {
    inner: Box<dyn serialport::SerialPort>,
}

impl SerialPort {
    pub fn open(path: &str, baud: u32) -> io::Result<Self> {
        let mut inner = serialport::new(path, baud)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            .stop_bits(serialport::StopBits::One)
            .flow_control(serialport::FlowControl::None)
            .timeout(Duration::from_millis(100))
            .open()
            .map_err(io::Error::other)?;
        inner
            .write_data_terminal_ready(false)
            .map_err(io::Error::other)?;
        inner
            .write_request_to_send(false)
            .map_err(io::Error::other)?;
        std::thread::sleep(Duration::from_millis(100));
        let mut p = Self { inner };
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
