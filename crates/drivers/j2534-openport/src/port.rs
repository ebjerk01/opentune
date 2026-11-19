//! The byte transport under the OpenPort 2.0 session: the CDC-ACM serial node
//! on real hardware, or the offline fake ([`crate::fake`]) in tests.
use opentune_core::{Error, Result};
use std::io::{Read, Write};
use std::time::Duration;

/// A bidirectional byte stream to the adapter.
pub trait Op2Port {
    /// Write all of `bytes` (one command line plus its payload) and flush.
    fn write_all(&mut self, bytes: &[u8]) -> Result<()>;
    /// Read whatever is available into `buf`, waiting at most `timeout`.
    /// `Ok(0)` means nothing arrived in time; it is not an error.
    fn read_some(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize>;
}

/// The OP2's USB CDC-ACM interface (`/dev/ttyACM*` on Linux, `COMn` on
/// Windows, `/dev/cu.usbmodem*` on macOS). The line coding is ignored by the
/// device; DTR and RTS are asserted as the reference drivers do.
pub struct SerialOp2Port {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialOp2Port {
    pub fn open(path: &str) -> Result<Self> {
        let mut port = serialport::new(path, 115_200)
            .timeout(Duration::from_millis(20))
            .open()
            .map_err(|e| Error::Config(format!("cannot open OpenPort 2.0 port {path}: {e}")))?;
        // Some CDC-ACM firmwares only send while DTR is set; not every port
        // type supports the lines (a pty in tests does not), so best effort.
        let _ = port.write_data_terminal_ready(true);
        let _ = port.write_request_to_send(true);
        Ok(Self { port })
    }
}

impl Op2Port for SerialOp2Port {
    fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        self.port.write_all(bytes).map_err(Error::Io)?;
        self.port.flush().map_err(Error::Io)
    }

    fn read_some(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let _ = self.port.set_timeout(timeout.max(Duration::from_millis(1)));
        match self.port.read(buf) {
            Ok(n) => Ok(n),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(0)
            }
            Err(e) => Err(Error::Io(e)),
        }
    }
}
