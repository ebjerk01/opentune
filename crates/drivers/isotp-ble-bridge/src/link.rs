//! The byte transport under a bridge session: BLE GATT (feature `btleplug`),
//! the firmware's USB serial port, or the in-process [`mock`](crate::mock).
//!
//! A [`Link`] only moves bytes; the [`Bridge`](crate::Bridge) owns framing,
//! splitting and reassembly through the dependency-free [`codec`](crate::codec).
use opentune_core::{Error, Result};
use std::time::Duration;

/// How a link delimits data, which decides the codec path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Framing {
    /// Discrete GATT values (writes and notifications) of at most
    /// `att_payload` bytes (negotiated ATT MTU − 3); long frames use the
    /// split-packet scheme.
    Ble { att_payload: usize },
    /// A byte stream (USB serial); frames are delimited by their header only.
    Stream,
}

/// A byte transport to the bridge.
pub trait Link: Send {
    /// How this link delimits data.
    fn framing(&self) -> Framing;
    /// Send one BLE value (≤ `att_payload` bytes) or a run of stream bytes.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    /// The next BLE notification value, or whatever stream bytes arrived,
    /// waiting at most `timeout` (`Duration::ZERO` polls). `Ok(None)` on
    /// timeout.
    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>>;
    /// A short human description (port path, BLE name/address).
    fn describe(&self) -> String;
}

/// The firmware's USB serial port (the Macchina A0 / clone's USB-UART bridge,
/// [`SERIAL_BAUD`](crate::codec::SERIAL_BAUD) 8N1). The firmware only listens
/// on it while no BLE central is connected.
pub struct SerialLink {
    port: Box<dyn serialport::SerialPort>,
    path: String,
}

impl SerialLink {
    /// Open `path` at `baud`. DTR and RTS are driven to the same level so the
    /// two-transistor auto-reset circuit of common ESP32 boards neither resets
    /// the device nor enters its ROM loader.
    pub fn open(path: &str, baud: u32) -> Result<Self> {
        let mut port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|e| Error::Io(std::io::Error::other(format!("open {path}: {e}"))))?;
        // Best effort: not every USB-UART exposes the modem lines.
        let _ = port.write_data_terminal_ready(false);
        let _ = port.write_request_to_send(false);
        Ok(Self {
            port,
            path: path.to_string(),
        })
    }
}

impl Link for SerialLink {
    fn framing(&self) -> Framing {
        Framing::Stream
    }

    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        self.port.write_all(bytes)?;
        self.port.flush()?;
        Ok(())
    }

    fn read(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let t = timeout.max(Duration::from_millis(1));
        self.port
            .set_timeout(t)
            .map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let mut buf = [0u8; 1024];
        match self.port.read(&mut buf) {
            Ok(0) => Ok(None),
            Ok(n) => Ok(Some(buf[..n].to_vec())),
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => Ok(None),
            Err(e) => Err(Error::Io(e)),
        }
    }

    fn describe(&self) -> String {
        format!("serial {}", self.path)
    }
}
