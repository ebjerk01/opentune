//! Adapter discovery, ELM327 auto-detect and the ELM327 battery link.
//!
//! What this module may send, and to what:
//!
//! - Only ELM327 **AT commands** (`ATI`, `ATZ`, `ATE0`, `ATRV`), and only to
//!   ports that look like an ELM327 (CH340 / CH9102 / CP210x bridges, "ELM" /
//!   "OBD" product strings, Bluetooth serial) or the port the user picked for
//!   the `elm327obd` driver. All of them are answered by the adapter itself;
//!   none is forwarded to the vehicle.
//! - K-line (KKL) cables, J2534 / OpenPort, slcan, SocketCAN and unknown
//!   serial ports are listed but never opened.
//! - No ECU request is ever sent from here.
//!
//! All serial I/O runs on background threads; the UI polls results and the
//! threads request a repaint when something changes.

use eframe::egui;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Driver id of the ELM327 serial OBD driver (see `topbar::DRIVERS`).
pub const ELM_DRIVER: &str = "elm327obd";

/// Serial bauds tried for an ELM327, most common first: 38400 (most USB
/// v1.x clones), 115200 (v2.x / STN-based), 9600 (old firmware), then the
/// faster rates some adapters are reconfigured to.
pub const ELM_BAUDS: &[u32] = &[38400, 115200, 9600, 57600, 230400, 500000];

/// Battery poll interval while connected.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Pure mapping / parsing (unit-tested)
// ---------------------------------------------------------------------------

/// Map a USB serial device to the driver id it most likely needs.
///
/// Product / manufacturer strings win over VID:PID, because the same bridge
/// chip (FTDI, CH340) is used by very different cables.
pub fn guess_driver(
    vid: u16,
    pid: u16,
    manufacturer: Option<&str>,
    product: Option<&str>,
) -> Option<&'static str> {
    let text = format!(
        "{} {}",
        manufacturer.unwrap_or_default(),
        product.unwrap_or_default()
    )
    .to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    if has(&["tactrix", "openport"]) || (vid, pid) == (0x0403, 0xcc4c) {
        return Some("openport2");
    }
    if has(&["elm327", "elm 327", "obdlink", "obdii", "obd-ii", "obd2"]) {
        return Some(ELM_DRIVER);
    }
    if has(&["canable", "slcan", "cantact"]) || (vid, pid) == (0x16d0, 0x117e) {
        return Some("slcan");
    }
    if has(&["kkl", "k-line", "kline", "409.1"]) {
        return Some("kkl");
    }
    match (vid, pid) {
        // FTDI FT232R / FT-X / FT2232 / FT4232 / FT232H: the classic VAG
        // KKL 409.1 cables. (Some ELM327s use FTDI too; those say so in
        // their product string, handled above.)
        (0x0403, 0x6001 | 0x6010 | 0x6011 | 0x6014 | 0x6015) => Some("kkl"),
        // WCH CH340 / CH341 / CH9102 and SiLabs CP210x: cheap USB ELM327s.
        (0x1a86, 0x7523 | 0x7522 | 0x5523 | 0x55d4) => Some(ELM_DRIVER),
        (0x10c4, 0xea60) => Some(ELM_DRIVER),
        _ => None,
    }
}

/// Whether a serial port is worth listing. Built-in UARTs (`/dev/ttyS*`,
/// PCI, unknown) are noise; on macOS each device appears twice and the
/// `/dev/cu.*` twin is the one to open.
pub fn keep_port(name: &str, kind: &Transport) -> bool {
    if name.starts_with("/dev/tty.") {
        return false;
    }
    match kind {
        Transport::Usb | Transport::Bluetooth => true,
        Transport::Other => name.contains("rfcomm"),
        Transport::SocketCan => true,
    }
}

/// `/sys/class/net/<if>/type` of a CAN netdev is `ARPHRD_CAN` = 280.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn is_can_type(sysfs_type: &str) -> bool {
    sysfs_type.trim() == "280"
}

/// Parse an `ATRV` reply ("12.6V", "12.6 V\r>", "ATRV\r13.8V\r\r>") into
/// volts. Echo, prompt and whitespace are ignored; anything without a
/// plausible voltage (`?`, `NO DATA`, garbage) is `None`.
pub fn parse_atrv(reply: &str) -> Option<f32> {
    for line in reply.split(['\r', '\n', '>']) {
        let line = line.trim();
        if line.is_empty() || line.to_ascii_uppercase().starts_with("AT") {
            continue;
        }
        let num: String = line
            .trim_end_matches(['V', 'v'])
            .trim()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        if num.is_empty() || !num.chars().all(|c| c.is_ascii_digit() || c == '.') {
            continue;
        }
        if let Ok(v) = num.parse::<f32>() {
            if (0.0..=40.0).contains(&v) {
                return Some(v);
            }
        }
    }
    None
}

/// The identification line of an ELM327 reply to `ATI` / `ATZ`
/// ("ATZ\r\r\rELM327 v1.5\r\r>" -> "ELM327 v1.5"); `None` if it is not one.
pub fn elm_ident(reply: &str) -> Option<String> {
    reply
        .split(['\r', '\n', '>'])
        .map(str::trim)
        .find(|l| l.to_ascii_uppercase().contains("ELM"))
        .map(str::to_string)
}

/// A reply that ends at the ELM prompt (`>`): the adapter is listening at
/// this baud even if the command was garbled (`?`).
pub fn has_prompt(reply: &str) -> bool {
    reply.contains('>')
}

/// A CAN id typed in the adapter settings: blank (= definition default) or
/// hex, optionally `0x`-prefixed, up to 29 bits.
pub fn valid_can_id(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return true;
    }
    let h = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    !h.is_empty() && h.len() <= 8 && u32::from_str_radix(h, 16).is_ok_and(|v| v <= 0x1FFF_FFFF)
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transport {
    Usb,
    Bluetooth,
    /// Linux only (SocketCAN netdevs).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    SocketCan,
    Other,
}

/// A discovered adapter (serial port or CAN interface).
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Port path / interface name ("/dev/ttyUSB0", "COM3", "can0").
    pub port: String,
    pub transport: Transport,
    /// Likely driver id, if recognised.
    pub driver: Option<&'static str>,
    /// Human detail: chip / product, VID:PID, CAN netdev driver and state.
    pub detail: String,
    /// ELM327 identification (`ATI`) if probing confirmed one.
    pub elm: Option<String>,
    /// Baud the ELM327 answered at.
    pub elm_baud: Option<u32>,
}

impl Candidate {
    /// May this port be sent ELM327 AT commands?
    pub fn may_probe(&self) -> bool {
        self.driver == Some(ELM_DRIVER)
            && matches!(self.transport, Transport::Usb | Transport::Bluetooth)
    }
}

/// Detection preference (higher is better; 0 = never auto-picked).
pub fn score(c: &Candidate, probed: bool) -> u32 {
    if c.elm.is_some() {
        return 100;
    }
    match c.driver {
        Some("openport2") => 80,
        Some("slcan") => 60,
        Some("socketcan") => 55,
        Some("kkl") => 40,
        // An unconfirmed ELM guess is weak; after a failed probe weaker still.
        Some(ELM_DRIVER) if !probed => 30,
        Some(ELM_DRIVER) => 10,
        Some(_) => 20,
        None => 0,
    }
}

/// The best candidate to put in the cable select, if any is recognised.
pub fn best(cands: &[Candidate], probed: bool) -> Option<&Candidate> {
    cands
        .iter()
        .filter(|c| score(c, probed) > 0)
        .fold(None, |acc: Option<&Candidate>, c| match acc {
            Some(a) if score(a, probed) >= score(c, probed) => Some(a),
            _ => Some(c),
        })
}

fn serial_candidates(errors: &mut Vec<String>) -> Vec<Candidate> {
    let ports = match serialport::available_ports() {
        Ok(p) => p,
        Err(e) => {
            errors.push(format!("serial ports: {e}"));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for p in ports {
        let (transport, driver, detail) = match &p.port_type {
            serialport::SerialPortType::UsbPort(u) => {
                let name = [u.manufacturer.as_deref(), u.product.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ");
                let ids = format!("{:04x}:{:04x}", u.vid, u.pid);
                let detail = if name.is_empty() {
                    format!("USB {ids}")
                } else {
                    format!("{name} ({ids})")
                };
                (
                    Transport::Usb,
                    guess_driver(
                        u.vid,
                        u.pid,
                        u.manufacturer.as_deref(),
                        u.product.as_deref(),
                    ),
                    detail,
                )
            }
            serialport::SerialPortType::BluetoothPort => (
                Transport::Bluetooth,
                Some(ELM_DRIVER),
                "Bluetooth serial".to_string(),
            ),
            _ if p.port_name.contains("rfcomm") => (
                Transport::Bluetooth,
                Some(ELM_DRIVER),
                "Bluetooth serial (rfcomm)".to_string(),
            ),
            _ => (Transport::Other, None, "serial".to_string()),
        };
        if keep_port(&p.port_name, &transport) {
            out.push(Candidate {
                port: p.port_name,
                transport,
                driver,
                detail,
                elm: None,
                elm_baud: None,
            });
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn socketcan_candidates() -> Vec<Candidate> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else {
        return out;
    };
    for e in dir.flatten() {
        let path = e.path();
        let is_can = std::fs::read_to_string(path.join("type")).is_ok_and(|t| is_can_type(&t));
        if !is_can {
            continue;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let kdrv = std::fs::read_link(path.join("device/driver"))
            .ok()
            .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "virtual".into());
        let state = std::fs::read_to_string(path.join("operstate"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        out.push(Candidate {
            port: name,
            transport: Transport::SocketCan,
            driver: Some("socketcan"),
            detail: format!("SocketCAN ({kdrv}, {state})"),
            elm: None,
            elm_baud: None,
        });
    }
    out.sort_by(|a, b| a.port.cmp(&b.port));
    out
}

#[cfg(not(target_os = "linux"))]
fn socketcan_candidates() -> Vec<Candidate> {
    Vec::new()
}

/// List serial ports and CAN interfaces (no I/O to any device).
pub fn enumerate(errors: &mut Vec<String>) -> Vec<Candidate> {
    let mut v = serial_candidates(errors);
    v.extend(socketcan_candidates());
    v
}

// ---------------------------------------------------------------------------
// ELM327 serial I/O (AT commands only)
// ---------------------------------------------------------------------------

type Port = Box<dyn serialport::SerialPort>;

fn open(port: &str, baud: u32) -> Result<Port, String> {
    serialport::new(port, baud)
        .timeout(Duration::from_millis(50))
        .open()
        .map_err(|e| {
            let msg = e.to_string();
            if msg.to_ascii_lowercase().contains("permission denied") {
                format!(
                    "{port}: permission denied (on Linux, add your user to the dialout/uucp group)"
                )
            } else {
                format!("{port}: {msg}")
            }
        })
}

/// Send one AT command and read up to the `>` prompt (or the timeout).
fn at(port: &mut Port, cmd: &str, timeout: Duration) -> std::io::Result<String> {
    debug_assert!(cmd.starts_with("AT"), "only AT commands are sent");
    let _ = port.clear(serialport::ClearBuffer::Input);
    port.write_all(cmd.as_bytes())?;
    port.write_all(b"\r")?;
    port.flush()?;
    let deadline = Instant::now() + timeout;
    let mut out = Vec::new();
    let mut buf = [0u8; 128];
    while Instant::now() < deadline && out.len() < 4096 {
        match port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                out.extend_from_slice(&buf[..n]);
                if out.contains(&b'>') {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Find the baud an ELM327 answers `ATI` at. Returns the open port, baud and
/// identification. `first` (a remembered baud, 0 = none) is tried first.
fn find_elm(
    port: &str,
    first: u32,
    stop: Option<&AtomicBool>,
) -> Result<(Port, u32, String), String> {
    let mut bauds: Vec<u32> = Vec::new();
    if first != 0 {
        bauds.push(first);
    }
    bauds.extend(ELM_BAUDS.iter().copied().filter(|b| *b != first));
    for baud in bauds {
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            return Err("cancelled".into());
        }
        let mut p = open(port, baud)?;
        // A second ATI if the first was garbled by stale input ("?" + prompt).
        for _ in 0..2 {
            let r = at(&mut p, "ATI", Duration::from_millis(450))
                .map_err(|e| format!("{port}: {e}"))?;
            if let Some(id) = elm_ident(&r) {
                return Ok((p, baud, id));
            }
            if !has_prompt(&r) {
                break;
            }
        }
    }
    Err(format!("{port}: no ELM327 reply at {} baud", fmt_bauds()))
}

fn fmt_bauds() -> String {
    ELM_BAUDS
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Auto-detect (background)
// ---------------------------------------------------------------------------

/// Result of a scan.
#[derive(Clone, Debug, Default)]
pub struct DetectReport {
    pub candidates: Vec<Candidate>,
    /// Whether ELM-looking ports were probed with AT commands.
    pub probed: bool,
    pub errors: Vec<String>,
}

/// A port already open by the link: not probed again, reported as confirmed.
#[derive(Clone, Debug)]
pub struct BusyPort {
    pub port: String,
    pub ident: String,
    pub baud: u32,
}

/// Enumerate (and with `probe`, confirm ELM327s) on a background thread.
pub fn spawn_scan(
    ctx: egui::Context,
    probe: bool,
    busy: Option<BusyPort>,
    baud_hint: u32,
) -> Receiver<DetectReport> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("adapter-scan".into())
        .spawn(move || {
            let mut errors = Vec::new();
            let mut candidates = enumerate(&mut errors);
            if probe {
                for c in candidates.iter_mut().filter(|c| c.may_probe()) {
                    if let Some(b) = busy.as_ref().filter(|b| b.port == c.port) {
                        c.elm = Some(b.ident.clone());
                        c.elm_baud = Some(b.baud);
                        continue;
                    }
                    match find_elm(&c.port, baud_hint, None) {
                        Ok((_port, baud, id)) => {
                            c.elm = Some(id);
                            c.elm_baud = Some(baud);
                        }
                        Err(e) => errors.push(e),
                    }
                }
            }
            let _ = tx.send(DetectReport {
                candidates,
                probed: probe,
                errors,
            });
            ctx.request_repaint();
        })
        .expect("spawn adapter-scan thread");
    rx
}

// ---------------------------------------------------------------------------
// ELM327 link (background): connect, init, poll ATRV
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LinkStatus {
    #[default]
    Idle,
    Connecting,
    Connected,
}

/// Live adapter state shown in the top bar.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkState {
    pub status: LinkStatus,
    pub port: String,
    pub baud: u32,
    /// ELM327 identification (`ATI`).
    pub ident: Option<String>,
    pub battery_v: Option<f32>,
    /// Why the last connection ended / failed.
    pub error: Option<String>,
}

/// Owner of the link worker. Each connection gets its own state, so a
/// worker that is still winding down never overwrites a newer connection.
#[derive(Default)]
pub struct Link {
    state: Arc<Mutex<LinkState>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Link {
    pub fn snapshot(&self) -> LinkState {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Open `port`, init the ELM327 (ATZ, ATE0) and poll ATRV every 2 s.
    pub fn connect(&mut self, ctx: egui::Context, port: String, baud_hint: u32) {
        let previous = self.shutdown();
        self.stop = Arc::new(AtomicBool::new(false));
        self.state = Arc::new(Mutex::new(LinkState {
            status: LinkStatus::Connecting,
            port: port.clone(),
            ..Default::default()
        }));
        let (state, stop) = (self.state.clone(), self.stop.clone());
        self.worker = thread::Builder::new()
            .name("elm327-link".into())
            .spawn(move || {
                // The previous worker must have released the port first.
                if let Some(h) = previous {
                    let _ = h.join();
                }
                run_link(&ctx, &port, baud_hint, &state, &stop);
            })
            .ok();
    }

    pub fn disconnect(&mut self) {
        // The worker exits within ~0.1 s (or after its current command); it
        // is joined by the next connect, never on the UI thread.
        let previous = self.shutdown();
        self.state = Arc::new(Mutex::new(LinkState::default()));
        if let Some(h) = previous {
            let _ = thread::Builder::new()
                .name("elm327-link-join".into())
                .spawn(move || {
                    let _ = h.join();
                });
        }
    }

    fn shutdown(&mut self) -> Option<JoinHandle<()>> {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take()
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_link(
    ctx: &egui::Context,
    port: &str,
    baud_hint: u32,
    state: &Mutex<LinkState>,
    stop: &AtomicBool,
) {
    let set = |f: &dyn Fn(&mut LinkState)| {
        if let Ok(mut s) = state.lock() {
            f(&mut s);
        }
        ctx.request_repaint();
    };
    let fail = |reason: String| {
        set(&|s| {
            *s = LinkState {
                port: s.port.clone(),
                error: Some(reason.clone()),
                ..Default::default()
            }
        })
    };
    let stopped = || stop.load(Ordering::Relaxed);

    let (mut p, baud, ident) = match find_elm(port, baud_hint, Some(stop)) {
        Ok(x) => x,
        Err(e) => {
            if !stopped() {
                fail(e);
            }
            return;
        }
    };
    // Reset (the reply is the banner; ~1 s on most adapters), echo off.
    let init = at(&mut p, "ATZ", Duration::from_millis(2500))
        .and_then(|_| at(&mut p, "ATE0", Duration::from_millis(800)));
    match init {
        Ok(r) if r.to_ascii_uppercase().contains("OK") => {}
        Ok(r) => {
            fail(format!(
                "{port}: ELM327 init failed (ATE0 replied {:?})",
                r.trim()
            ));
            return;
        }
        Err(e) => {
            fail(format!("{port}: {e}"));
            return;
        }
    }
    set(&|s| {
        s.status = LinkStatus::Connected;
        s.baud = baud;
        s.ident = Some(ident.clone());
    });

    let mut misses = 0;
    while !stopped() {
        match at(&mut p, "ATRV", Duration::from_millis(1000)) {
            Ok(r) => match parse_atrv(&r) {
                Some(v) => {
                    misses = 0;
                    let changed = state
                        .lock()
                        .map(|s| s.battery_v != Some(v))
                        .unwrap_or(false);
                    if changed {
                        set(&|s| s.battery_v = Some(v));
                    }
                }
                None => misses += 1,
            },
            Err(e) => {
                if !stopped() {
                    fail(format!("{port}: {e} (unplugged?)"));
                }
                return;
            }
        }
        if misses >= 3 {
            fail(format!("{port}: adapter stopped answering ATRV"));
            return;
        }
        let until = Instant::now() + POLL_INTERVAL;
        while Instant::now() < until && !stopped() {
            thread::sleep(Duration::from_millis(100));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_guesses() {
        // Bridge chips.
        assert_eq!(
            guess_driver(0x0403, 0x6001, Some("FTDI"), Some("FT232R USB UART")),
            Some("kkl")
        );
        assert_eq!(guess_driver(0x0403, 0x6015, None, None), Some("kkl"));
        assert_eq!(
            guess_driver(0x1a86, 0x7523, None, Some("USB2.0-Serial")),
            Some(ELM_DRIVER)
        );
        assert_eq!(guess_driver(0x1a86, 0x55d4, None, None), Some(ELM_DRIVER));
        assert_eq!(
            guess_driver(
                0x10c4,
                0xea60,
                Some("Silicon Labs"),
                Some("CP2102 USB to UART Bridge Controller")
            ),
            Some(ELM_DRIVER)
        );
        // Product strings win over the bridge chip.
        assert_eq!(
            guess_driver(0x0403, 0x6015, Some("ScanTool.net"), Some("OBDLink SX")),
            Some(ELM_DRIVER)
        );
        assert_eq!(
            guess_driver(0x0403, 0x6001, None, Some("ELM327 USB")),
            Some(ELM_DRIVER)
        );
        assert_eq!(
            guess_driver(0x0403, 0xcc4c, Some("Tactrix Inc."), Some("OpenPort 2.0")),
            Some("openport2")
        );
        assert_eq!(guess_driver(0x0403, 0xcc4c, None, None), Some("openport2"));
        assert_eq!(
            guess_driver(0x16d0, 0x117e, None, Some("CANable")),
            Some("slcan")
        );
        assert_eq!(
            guess_driver(0x1234, 0x0001, None, Some("VAG KKL 409.1")),
            Some("kkl")
        );
        // Unknown.
        assert_eq!(guess_driver(0x067b, 0x2303, Some("Prolific"), None), None);
        assert_eq!(guess_driver(0x1234, 0x5678, None, None), None);
    }

    #[test]
    fn port_filter_and_can_type() {
        assert!(keep_port("/dev/ttyUSB0", &Transport::Usb));
        assert!(keep_port("COM3", &Transport::Usb));
        assert!(!keep_port("/dev/ttyS0", &Transport::Other));
        assert!(keep_port("/dev/rfcomm0", &Transport::Other));
        assert!(!keep_port("/dev/tty.usbserial-1", &Transport::Usb));
        assert!(keep_port("/dev/cu.usbserial-1", &Transport::Usb));
        assert!(is_can_type("280\n"));
        assert!(!is_can_type("1\n"));
        assert!(!is_can_type(""));
    }

    #[test]
    fn atrv_replies() {
        assert_eq!(parse_atrv("12.6V"), Some(12.6));
        assert_eq!(parse_atrv("12.6 V\r>"), Some(12.6));
        assert_eq!(parse_atrv("ATRV\r13.8V\r\r>"), Some(13.8));
        assert_eq!(parse_atrv("\r\n 11.9v \r\n>"), Some(11.9));
        assert_eq!(parse_atrv("14V\r>"), Some(14.0));
        assert_eq!(parse_atrv("?\r\r>"), None);
        assert_eq!(parse_atrv("NO DATA\r>"), None);
        assert_eq!(parse_atrv(""), None);
        assert_eq!(parse_atrv(">"), None);
        assert_eq!(parse_atrv("99.9V>"), None);
        assert_eq!(parse_atrv("1.2.3V>"), None);
    }

    #[test]
    fn elm_identification() {
        assert_eq!(
            elm_ident("ATZ\r\r\rELM327 v1.5\r\r>").as_deref(),
            Some("ELM327 v1.5")
        );
        assert_eq!(elm_ident("ELM327 v2.1\r>").as_deref(), Some("ELM327 v2.1"));
        assert_eq!(elm_ident("?\r>"), None);
        assert_eq!(elm_ident(""), None);
        assert!(has_prompt("?\r>"));
        assert!(!has_prompt("\u{0}\u{ff}garbage"));
    }

    #[test]
    fn can_ids() {
        assert!(valid_can_id(""));
        assert!(valid_can_id("7E0"));
        assert!(valid_can_id("0x7e8"));
        assert!(valid_can_id("18DA10F1"));
        assert!(!valid_can_id("0x"));
        assert!(!valid_can_id("7G0"));
        assert!(!valid_can_id("20000000"));
        assert!(!valid_can_id("123456789"));
    }

    fn cand(port: &str, driver: Option<&'static str>, elm: bool) -> Candidate {
        Candidate {
            port: port.into(),
            transport: Transport::Usb,
            driver,
            detail: String::new(),
            elm: elm.then(|| "ELM327 v1.5".into()),
            elm_baud: elm.then_some(38400),
        }
    }

    #[test]
    fn best_candidate() {
        let v = vec![
            cand("/dev/ttyUSB0", Some("kkl"), false),
            cand("/dev/ttyUSB1", Some(ELM_DRIVER), false),
            cand("/dev/ttyUSB2", None, false),
        ];
        // Not probed: KKL (40) beats an unconfirmed ELM guess (30).
        assert_eq!(best(&v, false).unwrap().port, "/dev/ttyUSB0");
        let mut w = v.clone();
        w[1].elm = Some("ELM327 v1.5".into());
        assert_eq!(best(&w, true).unwrap().port, "/dev/ttyUSB1");
        assert!(best(&[cand("/dev/ttyACM0", None, false)], true).is_none());
        assert!(best(&[], false).is_none());
        // Equal scores keep the first.
        let two = vec![cand("a", Some("kkl"), false), cand("b", Some("kkl"), false)];
        assert_eq!(best(&two, false).unwrap().port, "a");
        // Only ELM-looking USB / Bluetooth ports may be probed.
        assert!(cand("x", Some(ELM_DRIVER), false).may_probe());
        assert!(!cand("x", Some("kkl"), false).may_probe());
        assert!(!cand("x", None, false).may_probe());
    }
}
