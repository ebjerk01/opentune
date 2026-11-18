//! The USB pipe to an EFP02C, behind a small trait so the protocol logic can be
//! tested against a scripted replay of captured transfers ([`Replay`]) and run
//! on real hardware through [`nusb`] (pure Rust, no libusb).
use opentune_core::{Error, Result};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// One bulk OUT/IN pipe pair to the box.
pub trait Efp02cTransport: Send {
    /// Send one OUT transfer (one or more 16-byte records).
    fn write(&mut self, data: &[u8]) -> Result<()>;
    /// Receive one IN transfer, waiting up to `timeout_ms`. An empty vector
    /// means nothing arrived in time (not an error).
    fn read(&mut self, timeout_ms: u64) -> Result<Vec<u8>>;
    /// Human description (bus/address), for the UI.
    fn describe(&self) -> String {
        "EFP02C".into()
    }
}

/// One scripted step of a [`Replay`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The driver must write exactly these bytes next.
    Out(Vec<u8>),
    /// The next read returns these bytes (an empty vector = a read timeout).
    In(Vec<u8>),
}

/// A scripted transport that replays a capture: every write is checked against
/// the next expected [`Step::Out`], every read returns the next [`Step::In`].
/// A read with no scripted IN left returns nothing (a timeout); a mismatched
/// or unexpected write is an error naming both byte strings.
#[derive(Clone, Default)]
pub struct Replay {
    script: Arc<Mutex<VecDeque<Step>>>,
    written: Arc<Mutex<Vec<Vec<u8>>>>,
    /// When set, writes are recorded but not checked (reads still follow the
    /// script, skipping any `Out` steps).
    lenient: bool,
}

impl Replay {
    /// A strict replay of `steps`.
    pub fn new(steps: Vec<Step>) -> Self {
        Self {
            script: Arc::new(Mutex::new(steps.into())),
            ..Self::default()
        }
    }

    /// A replay that records writes without checking them.
    pub fn lenient(steps: Vec<Step>) -> Self {
        Self {
            lenient: true,
            ..Self::new(steps)
        }
    }

    /// Every OUT transfer written so far (shared with clones).
    pub fn written(&self) -> Vec<Vec<u8>> {
        self.written.lock().map(|w| w.clone()).unwrap_or_default()
    }

    /// Steps not consumed yet.
    pub fn remaining(&self) -> usize {
        self.script.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Append steps (e.g. bus traffic after the init sequence).
    pub fn push(&self, step: Step) {
        if let Ok(mut s) = self.script.lock() {
            s.push_back(step);
        }
    }
}

fn hex(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

impl Efp02cTransport for Replay {
    fn write(&mut self, data: &[u8]) -> Result<()> {
        if let Ok(mut w) = self.written.lock() {
            w.push(data.to_vec());
        }
        if self.lenient {
            return Ok(());
        }
        let mut s = self
            .script
            .lock()
            .map_err(|_| Error::Protocol("replay poisoned".into()))?;
        match s.pop_front() {
            Some(Step::Out(want)) if want == data => Ok(()),
            Some(Step::Out(want)) => Err(Error::Protocol(format!(
                "replay: wrote [{}], capture has [{}]",
                hex(data),
                hex(&want)
            ))),
            other => Err(Error::Protocol(format!(
                "replay: unexpected write [{}] (next step {other:?})",
                hex(data)
            ))),
        }
    }

    fn read(&mut self, _timeout_ms: u64) -> Result<Vec<u8>> {
        let mut s = self
            .script
            .lock()
            .map_err(|_| Error::Protocol("replay poisoned".into()))?;
        if self.lenient {
            while matches!(s.front(), Some(Step::Out(_))) {
                s.pop_front();
            }
        }
        match s.front() {
            Some(Step::In(_)) => match s.pop_front() {
                Some(Step::In(b)) => Ok(b),
                _ => Ok(Vec::new()),
            },
            // Nothing scripted for this read, or the driver must write first.
            _ => Ok(Vec::new()),
        }
    }

    fn describe(&self) -> String {
        "EFP02C (replay)".into()
    }
}

/// Open the real device through `nusb`. `index` picks among several attached
/// EFP02C boxes (0 = first in enumeration order).
#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
pub fn open_usb(index: usize) -> Result<Box<dyn Efp02cTransport>> {
    usb::NusbTransport::open(index).map(|t| Box::new(t) as Box<dyn Efp02cTransport>)
}

/// USB access is only built for Linux, macOS and Windows.
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn open_usb(_index: usize) -> Result<Box<dyn Efp02cTransport>> {
    Err(Error::Unsupported(
        "EFP02C USB access is built for Linux, macOS and Windows only".into(),
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
mod usb {
    use super::Efp02cTransport;
    use crate::proto::{EP_IN, EP_OUT, RECORD_LEN, USB_INTERFACE, USB_PID, USB_VID};
    use nusb::transfer::{Buffer, Bulk, In, Out, TransferError};
    use nusb::{Endpoint, MaybeFuture};
    use opentune_core::{Error, Result};
    use std::time::Duration;

    /// IN request size: a 32-record reply plus its trailer fits (64 records).
    const IN_LEN: usize = 64 * RECORD_LEN;
    /// OUT transfers are tiny; the box answers them at once.
    const OUT_TIMEOUT: Duration = Duration::from_millis(1000);

    pub struct NusbTransport {
        out: Endpoint<Bulk, Out>,
        inp: Endpoint<Bulk, In>,
        describe: String,
    }

    fn usb_err(what: &str, e: impl std::fmt::Display) -> Error {
        Error::Io(std::io::Error::other(format!("EFP02C {what}: {e}")))
    }

    impl NusbTransport {
        pub fn open(index: usize) -> Result<Self> {
            let mut found: Vec<nusb::DeviceInfo> = nusb::list_devices()
                .wait()
                .map_err(|e| usb_err("USB enumeration", e))?
                .filter(|d| d.vendor_id() == USB_VID && d.product_id() == USB_PID)
                .collect();
            let count = found.len();
            if index >= count {
                return Err(Error::Config(format!(
                    "no EFP02C (USB {USB_VID:04x}:{USB_PID:04x}) at index {index}: {count} attached{}",
                    if cfg!(windows) {
                        " (on Windows the device needs the WinUSB driver bound, e.g. with Zadig)"
                    } else if cfg!(target_os = "linux") {
                        " (on Linux check the udev rule in docs/DRIVERS.md)"
                    } else {
                        ""
                    }
                )));
            }
            let info = found.swap_remove(index);
            let describe = format!(
                "EFP02C on USB bus {} address {}",
                info.bus_id(),
                info.device_address()
            );
            let dev = info.open().wait().map_err(|e| usb_err("open", e))?;
            let intf = dev
                .detach_and_claim_interface(USB_INTERFACE)
                .wait()
                .map_err(|e| usb_err("claim interface", e))?;
            let out = intf
                .endpoint::<Bulk, Out>(EP_OUT)
                .map_err(|e| usb_err("OUT endpoint", e))?;
            let inp = intf
                .endpoint::<Bulk, In>(EP_IN)
                .map_err(|e| usb_err("IN endpoint", e))?;
            Ok(Self { out, inp, describe })
        }
    }

    impl Efp02cTransport for NusbTransport {
        fn write(&mut self, data: &[u8]) -> Result<()> {
            let c = self
                .out
                .transfer_blocking(Buffer::from(data.to_vec()), OUT_TIMEOUT);
            match c.status {
                Ok(()) => Ok(()),
                Err(TransferError::Cancelled) => {
                    Err(Error::Timeout(OUT_TIMEOUT.as_millis() as u64))
                }
                Err(e) => Err(usb_err("write", e)),
            }
        }

        fn read(&mut self, timeout_ms: u64) -> Result<Vec<u8>> {
            let c = self
                .inp
                .transfer_blocking(Buffer::new(IN_LEN), Duration::from_millis(timeout_ms));
            match c.status {
                Ok(()) => Ok(c.buffer.to_vec()),
                // Timed out: keep whatever arrived before the cancel.
                Err(TransferError::Cancelled) => Ok(c.buffer.to_vec()),
                Err(e) => Err(usb_err("read", e)),
            }
        }

        fn describe(&self) -> String {
            self.describe.clone()
        }
    }
}
