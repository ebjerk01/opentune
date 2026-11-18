//! A scripted slcan adapter for tests: speaks the LAWICEL line protocol to the
//! driver on one side and hands the CAN frames to a simulated bus on the
//! other. No hardware, synthetic.
//!
//! It answers `S`/`O`/`C`/`L`/`V`/`N` commands with `\r`, acknowledges each
//! transmitted frame with `z\r` / `Z\r` like a CANable, refuses frames while
//! the channel is closed (BEL), and encodes the bus's replies as
//! `t<iii><l><dd…>\r` / `T<iiiiiiii><l><dd…>\r` lines. An empty receive buffer
//! reads as a timeout at once, so tests do not wait.
use crate::SlcanPort;
use opentune_core::protocol::tp20sim::Tp20Ecu;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

/// The simulated bus: a frame the host transmitted in, frames on the bus out.
pub type Bus = Box<dyn FnMut(u32, &[u8]) -> Vec<(u32, Vec<u8>)> + Send>;

struct State {
    bus: Bus,
    line: Vec<u8>,
    out: VecDeque<u8>,
    lines: Vec<String>,
    open: bool,
}

/// The simulated adapter. Clones share state, so a test keeps one handle to
/// inspect what the driver wrote.
#[derive(Clone)]
pub struct SimSlcanPort {
    state: Arc<Mutex<State>>,
}

impl SimSlcanPort {
    pub fn new(bus: Bus) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                bus,
                line: Vec::new(),
                out: VecDeque::new(),
                lines: Vec::new(),
                open: false,
            })),
        }
    }

    /// An adapter on a bus with one simulated TP2.0 module.
    pub fn tp20(ecu: Arc<Mutex<Tp20Ecu>>) -> Self {
        Self::new(Box::new(move |id, data| {
            ecu.lock()
                .unwrap_or_else(|e| e.into_inner())
                .on_frame(id, data)
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every line the driver wrote, without the `\r`.
    pub fn lines(&self) -> Vec<String> {
        self.state().lines.clone()
    }

    /// Whether the adapter's CAN channel is open (`O` seen, no `C` since).
    pub fn is_open(&self) -> bool {
        self.state().open
    }
}

fn parse_frame(line: &str) -> Option<(u32, Vec<u8>)> {
    let id_len = match line.as_bytes().first()? {
        b't' => 3,
        b'T' => 8,
        _ => return None,
    };
    let id = u32::from_str_radix(line.get(1..1 + id_len)?, 16).ok()?;
    let dlc = usize::from_str_radix(line.get(1 + id_len..2 + id_len)?, 16).ok()?;
    let hex = line.get(2 + id_len..)?;
    if dlc > 8 || hex.len() < 2 * dlc {
        return None;
    }
    let data = (0..dlc)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    Some((id, data))
}

fn encode_frame(id: u32, data: &[u8]) -> String {
    let mut s = if id > 0x7FF {
        format!("T{id:08X}")
    } else {
        format!("t{id:03X}")
    };
    s.push_str(&format!("{:X}", data.len()));
    for b in data {
        s.push_str(&format!("{b:02X}"));
    }
    s.push('\r');
    s
}

impl State {
    fn handle_line(&mut self, line: String) {
        let reply: Vec<u8> = match line.as_bytes().first() {
            Some(b'O') | Some(b'L') => {
                self.open = true;
                b"\r".to_vec()
            }
            Some(b'C') => {
                self.open = false;
                b"\r".to_vec()
            }
            Some(b'S') | Some(b'N') => b"\r".to_vec(),
            Some(b'V') => b"V1013\r".to_vec(),
            Some(b't') | Some(b'T') => match parse_frame(&line) {
                Some((id, data)) if self.open => {
                    let mut r = if id > 0x7FF {
                        b"Z\r".to_vec()
                    } else {
                        b"z\r".to_vec()
                    };
                    for (rid, rdata) in (self.bus)(id, &data) {
                        r.extend_from_slice(encode_frame(rid, &rdata).as_bytes());
                    }
                    r
                }
                _ => vec![0x07],
            },
            _ => vec![0x07],
        };
        self.lines.push(line);
        self.out.extend(reply);
    }
}

impl Read for SimSlcanPort {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut st = self.state();
        if st.out.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "no data from the simulated adapter",
            ));
        }
        let n = buf.len().min(st.out.len());
        for (i, b) in st.out.drain(..n).enumerate() {
            buf[i] = b;
        }
        Ok(n)
    }
}

impl Write for SimSlcanPort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut st = self.state();
        for &b in buf {
            if b == b'\r' {
                let line = String::from_utf8_lossy(&std::mem::take(&mut st.line)).into_owned();
                st.handle_line(line);
            } else {
                st.line.push(b);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl SlcanPort for SimSlcanPort {
    fn set_read_timeout(&mut self, _timeout: Duration) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_lines_roundtrip() {
        assert_eq!(
            parse_frame("t20070AC00010000301"),
            Some((0x200, vec![0x0A, 0xC0, 0x00, 0x10, 0x00, 0x03, 0x01]))
        );
        assert_eq!(encode_frame(0x300, &[0xA1, 0x0F]), "t3002A10F\r");
        assert_eq!(encode_frame(0x18DA_F110, &[1]), "T18DAF110101\r");
        assert_eq!(parse_frame("t2009"), None);
    }
}
