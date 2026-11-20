//! Interface drivers: what this build can talk to, from the hub's driver
//! registry (`opentune_hub::drivers::registry`, so the Bench-tier drivers
//! appear only in alpha builds), with a "Probe" that asks the app's adapter
//! for a fresh handshake with one cable ([`AdapterAccess::probe`]: nothing is
//! sent to an ECU) and shows what the adapter reported and which panels its
//! capabilities allow (as the webview app's `ot-caps.js` gates them).
//!
//! The card notes below are written from each driver crate's own
//! documentation (`crates/drivers/*/src/lib.rs`, `docs/DRIVERS.md`) and its
//! declared `Capabilities`. A validation state is only shown where the
//! driver's source states one; nothing here claims hardware verification the
//! source does not. A registered driver without a note still gets a card.

use crate::host::{AdapterAccess, Capabilities, JobHandle, Jobs};
use crate::icons;
use crate::theme::{self, font, Tokens, Tone, DISPLAY, DISPLAY_MEDIUM, MONO_MEDIUM, UI_MEDIUM};
use crate::widgets::caps::{self, Need};
use eframe::egui::{self, Align, CornerRadius, Label, Layout, Margin, RichText, Stroke, Ui, Vec2};
use std::sync::{Arc, Mutex, OnceLock};

/// How a driver reaches the vehicle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    KLine,
    Can,
    J2534,
    Network,
    Bluetooth,
    Simulator,
}

impl Transport {
    fn label(self) -> &'static str {
        match self {
            Transport::KLine => "K-line",
            Transport::Can => "CAN",
            Transport::J2534 => "J2534",
            Transport::Network => "network",
            Transport::Bluetooth => "Bluetooth",
            Transport::Simulator => "simulator",
        }
    }
}

/// A validation state the driver's own documentation states.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    /// Not run against the real device yet (mock / fake / scripted tests).
    AwaitsHardware,
    /// The source calls itself unverified.
    Unverified,
    /// Tested against an in-process simulator over loopback only.
    LoopbackOnly,
    /// The adapter itself was checked, but no ECU exchange yet.
    AdapterChecked,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::AwaitsHardware => "awaits hardware",
            Status::Unverified => "unverified",
            Status::LoopbackOnly => "loopback-tested only",
            Status::AdapterChecked => "adapter checked, no ECU yet",
        }
    }
    fn tone(self) -> Tone {
        match self {
            Status::AdapterChecked => Tone::Accent,
            _ => Tone::Warn,
        }
    }
}

struct Driver {
    /// Registry id (`opentune drivers`, `--driver <id>`).
    id: &'static str,
    /// Source directory under `crates/` (for the drift test).
    #[cfg_attr(not(test), allow(dead_code))]
    crate_dir: &'static str,
    name: &'static str,
    icon: &'static str,
    summary: &'static str,
    transports: &'static [Transport],
    /// Only where the source states a platform restriction.
    platforms: Option<&'static str>,
    status: Option<Status>,
    /// Declared capabilities (`Interface::capabilities`).
    caps: &'static [&'static str],
}

use Transport::*;

const NOTES: &[Driver] = &[
    Driver {
        id: "elm327obd",
        crate_dir: "drivers/elm327",
        name: "ELM327 serial (OBD)",
        icon: icons::USB,
        summary: "USB ELM327 on ISO 15765-4: identify, read and clear DTCs, live OBD-II PIDs \
                  and a UDS block read. Not for flashing: the adapter cannot send multi-frame \
                  ISO-TP.",
        transports: &[Can],
        platforms: None,
        status: Some(Status::AdapterChecked),
        caps: &["CAN", "read", "battery V"],
    },
    Driver {
        id: "elm327",
        crate_dir: "drivers/kline-elm327",
        name: "ELM327 (CAN + K-line)",
        icon: icons::USB,
        summary: "ELM327 on a serial port for UDS or KWP2000, over ISO 15765 CAN or \
                  ISO 14230 / 9141 K-line; the adapter does the ISO-TP and K-line framing.",
        transports: &[Can, KLine],
        platforms: None,
        status: None,
        caps: &["CAN", "K-line", "read", "write", "battery V"],
    },
    Driver {
        id: "elm327net",
        crate_dir: "drivers/elm327net",
        name: "ELM327 over WiFi / Bluetooth",
        icon: icons::WIFI_HIGH,
        summary: "The same ELM327 dialect over a TCP socket (WiFi adapters) or a Bluetooth \
                  RFCOMM serial device.",
        transports: &[Network, Bluetooth, Can, KLine],
        platforms: None,
        status: Some(Status::AwaitsHardware),
        caps: &["CAN", "K-line", "read", "write", "battery V"],
    },
    Driver {
        id: "kkl",
        crate_dir: "drivers/kkl",
        name: "KKL 409.1 (raw K-line)",
        icon: icons::PLUG,
        summary: "Dumb FTDI / CH340 K-line cables: OpenTune does the fast or 5-baud init, \
                  the KWP2000 framing and the echo cancellation itself.",
        transports: &[KLine],
        platforms: None,
        status: None,
        caps: &["K-line", "read", "write"],
    },
    Driver {
        id: "kline-bridge",
        crate_dir: "drivers/kline-bridge",
        name: "Arduino K-line bridge",
        icon: icons::CIRCUITRY,
        summary: "OpenTune's Arduino firmware does the timing-critical K-line work (init, \
                  inter-byte timing, echo removal); the host frames KWP2000.",
        transports: &[KLine],
        platforms: None,
        status: None,
        caps: &["K-line", "read", "write"],
    },
    Driver {
        id: "slcan",
        crate_dir: "drivers/slcan",
        name: "slcan USB-CAN",
        icon: icons::PLUGS,
        summary: "LAWICEL ASCII serial CAN adapters (CANable / CANtact, CH340 clones) with \
                  host-side ISO-TP + UDS, or a VW TP2.0 channel for KWP2000.",
        transports: &[Can],
        platforms: None,
        status: None,
        caps: &["CAN", "TP2.0", "read", "write"],
    },
    Driver {
        id: "socketcan",
        crate_dir: "drivers/socketcan",
        name: "SocketCAN",
        icon: icons::PLUGS,
        summary: "Any Linux SocketCAN interface as a raw CAN link, with the core ISO-TP + UDS \
                  stack or a VW TP2.0 channel on top.",
        transports: &[Can],
        platforms: Some("Linux only"),
        status: None,
        caps: &["CAN", "TP2.0", "read", "write"],
    },
    Driver {
        id: "gsusb",
        crate_dir: "drivers/gsusb",
        name: "gs_usb / candleLight",
        icon: icons::PLUGS,
        summary: "candleLight-class USB-CAN adapters through the kernel gs_usb driver (as a \
                  SocketCAN interface); the host-frame codec is ready for a libusb backend.",
        transports: &[Can],
        platforms: Some("Linux (kernel gs_usb)"),
        status: Some(Status::AwaitsHardware),
        caps: &["CAN", "TP2.0", "read", "write"],
    },
    Driver {
        id: "j2534",
        crate_dir: "drivers/j2534",
        name: "J2534 PassThru",
        icon: icons::USB,
        summary: "Vendor-neutral SAE J2534-1 through the vendor's PassThru DLL (Mongoose, \
                  VNCI, SVCI, Scanmatik, clones); the device does ISO-TP.",
        transports: &[J2534, Can],
        platforms: Some("Windows (vendor DLL)"),
        status: Some(Status::AwaitsHardware),
        caps: &["CAN", "read", "write"],
    },
    Driver {
        id: "openport2",
        crate_dir: "drivers/j2534-openport",
        name: "Tactrix OpenPort 2.0",
        icon: icons::USB,
        summary: "Speaks the cable's ASCII protocol over USB CDC-ACM and opens its raw CAN \
                  channel; ISO-TP and UDS run on the host.",
        transports: &[Can],
        platforms: None,
        status: Some(Status::AwaitsHardware),
        caps: &["CAN", "read", "write", "battery V"],
    },
    Driver {
        id: "efp02c",
        crate_dir: "drivers/efp02c",
        name: "EFI Technology EFP02C",
        icon: icons::USB,
        summary: "USB-CAN box driven directly over USB (pure Rust, no vendor DLL). 1 Mbit/s \
                  and 11-bit ids only; ECU writes stay disabled until the protocol is \
                  confirmed.",
        transports: &[Can],
        platforms: Some("Windows (WinUSB) · Linux (udev rule)"),
        status: Some(Status::Unverified),
        caps: &["CAN", "read"],
    },
    Driver {
        id: "isotp-ble",
        crate_dir: "drivers/isotp-ble-bridge",
        name: "ISO-TP BLE bridge",
        icon: icons::BLUETOOTH,
        summary: "Host side of the open esp32-isotp-ble-bridge firmware: whole ISO-TP \
                  payloads over USB serial, or BLE GATT with the btleplug build feature.",
        transports: &[Bluetooth, Can],
        platforms: None,
        status: Some(Status::Unverified),
        caps: &["CAN", "read", "write"],
    },
    Driver {
        id: "doip",
        crate_dir: "drivers/doip",
        name: "DoIP (UDS over Ethernet)",
        icon: icons::NETWORK,
        summary: "ISO 13400: dials or discovers (UDP) the vehicle's DoIP entity, runs routing \
                  activation, then UDS over TCP. Module programming goes through the UDS \
                  module flasher.",
        transports: &[Network],
        platforms: None,
        status: Some(Status::LoopbackOnly),
        caps: &["DoIP", "read"],
    },
    Driver {
        id: "esp32-opentune",
        crate_dir: "drivers/esp32-opentune",
        name: "OpenTune ESP32-S3 interface",
        icon: icons::CPU,
        summary: "OpenTune's own board: CAN (TWAI), ISO-TP and K-line timing on the device, \
                  UDS / KWP2000 on the host, over a framed USB-CDC protocol.",
        transports: &[Can, KLine],
        platforms: None,
        status: Some(Status::Unverified),
        caps: &["CAN", "K-line", "read", "write", "battery V"],
    },
    Driver {
        id: "vcar",
        crate_dir: "drivers/vcar",
        name: "Virtual car",
        icon: icons::CAR,
        summary: "A whole simulated vehicle: UDS modules on a virtual 11-bit CAN bus, \
                  functional OBD-II and synthetic bus traffic, for offline testing and demos.",
        transports: &[Simulator, Can],
        platforms: None,
        status: None,
        caps: &["CAN", "battery V"],
    },
    Driver {
        id: "udssim",
        crate_dir: "opentune-udssim",
        name: "UDS simulator",
        icon: icons::CUBE,
        summary: "One in-process simulated ECU with a 64 KiB flash model: read and program it \
                  over UDS with the simulator's test-only seed/key.",
        transports: &[Simulator],
        platforms: None,
        status: None,
        caps: &["CAN", "read", "write", "battery V"],
    },
    Driver {
        id: "mock",
        crate_dir: "drivers/mock",
        name: "Mock / file driver",
        icon: icons::FILE,
        summary: "Treats a .bin on disk as an ECU's flash (and a JSON store as its coding), so \
                  the read / verify / write pipeline runs with no hardware.",
        transports: &[Simulator],
        platforms: None,
        status: None,
        caps: &["read", "write", "battery V"],
    },
];

// ---------------------------------------------------------------------------
// The registry and the probe (no UI)
// ---------------------------------------------------------------------------

/// The drivers this build registers, `(id, registry name)`, in registry
/// order.
pub(crate) fn registered() -> &'static [(&'static str, &'static str)] {
    static LIST: OnceLock<Vec<(&'static str, &'static str)>> = OnceLock::new();
    LIST.get_or_init(|| {
        opentune_hub::drivers::registry()
            .list()
            .iter()
            .map(|e| (e.id, e.display_name))
            .collect()
    })
}

/// Number of drivers this build registers (sidebar count).
pub fn count() -> usize {
    registered().len()
}

fn note_for(id: &str) -> Option<&'static Driver> {
    NOTES.iter().find(|d| d.id == id)
}

/// Notes of drivers this build does not register (Bench tier on a stable
/// build).
fn not_in_build() -> Vec<&'static Driver> {
    NOTES
        .iter()
        .filter(|d| !registered().iter().any(|(id, _)| *id == d.id))
        .collect()
}

/// Registered drivers that touch real hardware (not simulators).
fn hardware_count() -> usize {
    registered()
        .iter()
        .filter(|(id, _)| !note_for(id).is_some_and(|d| d.transports.contains(&Simulator)))
        .count()
}

/// A checked probe request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProbeArgs {
    pub driver: String,
    pub port: String,
    /// 0 = the driver's default.
    pub baud: u32,
    /// `hs` / `ms` (a switched ELM327's HS/MS-CAN switch), or empty.
    pub bus: String,
}

/// Check a probe request: only a driver this build registers (a Bench-tier
/// driver is not registered on a stable build, so it cannot be probed
/// there), a numeric baud rate, and a known bus.
pub(crate) fn probe_args(
    driver: &str,
    port: &str,
    baud: &str,
    bus: &str,
) -> Result<ProbeArgs, String> {
    let driver = driver.trim();
    if !registered().iter().any(|(id, _)| *id == driver) {
        return Err(format!("'{driver}' is not a driver this build registers"));
    }
    let baud = match baud.trim() {
        "" => 0,
        b => b
            .parse::<u32>()
            .ok()
            .filter(|v| *v > 0)
            .ok_or_else(|| format!("baud rate '{b}' is not a positive number"))?,
    };
    let bus = bus.trim().to_ascii_lowercase();
    if !matches!(bus.as_str(), "" | "hs" | "ms") {
        return Err(format!("bus '{bus}' must be hs or ms"));
    }
    Ok(ProbeArgs {
        driver: driver.to_string(),
        port: port.trim().to_string(),
        baud,
        bus,
    })
}

/// The probe's answer (`opentune_hub::drivers::probe_json`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ProbeReport {
    pub connected: bool,
    /// Another operation held the adapter: not "absent".
    pub busy: bool,
    pub error: Option<String>,
    pub driver: Option<String>,
    pub name: Option<String>,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub battery_mv: Option<u64>,
    pub caps: Option<Capabilities>,
}

pub(crate) fn parse_probe(v: &serde_json::Value) -> ProbeReport {
    let s = |k: &str| v[k].as_str().map(str::to_string);
    let caps = v["capabilities"].as_object().map(|c| {
        let b = |k: &str| c.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        Capabilities {
            can: b("can"),
            can_fd: b("can_fd"),
            kline: b("kline"),
            read_memory: b("read_memory"),
            write_memory: b("write_memory"),
            bench_boot: b("bench_boot"),
            battery_voltage: b("battery_voltage"),
            programming_voltage: b("programming_voltage"),
            vwtp20: b("vwtp20"),
            doip: b("doip"),
        }
    });
    ProbeReport {
        connected: v["connected"].as_bool().unwrap_or(false),
        busy: v["busy"].as_bool().unwrap_or(false),
        error: s("error"),
        driver: s("driver"),
        name: s("name"),
        firmware: s("firmware"),
        serial: s("serial"),
        battery_mv: v["battery_mv"].as_u64(),
        caps,
    }
}

/// Run the probe on the app's adapter (blocking: call it from a job).
pub(crate) fn probe_op(
    a: &Arc<dyn AdapterAccess>,
    args: &ProbeArgs,
) -> Result<ProbeReport, String> {
    a.probe(&args.driver, &args.port, args.baud, &args.bus)
        .map(|v| parse_probe(&v))
}

/// What the panels need, and whether a probed adapter has it
/// (`widgets::caps`, the same gate the panels use).
const USES: &[(&str, &[Need])] = &[
    ("Diagnostics (scan, live data, coding)", &[Need::Diag]),
    ("K-line modules", &[Need::KLine]),
    ("CAN capture", &[Need::Can]),
    ("ECU read (memory)", &[Need::ReadMemory]),
    ("Flashing / memory write", &[Need::WriteMemory]),
    ("Bench / boot-mode access", &[Need::BenchBoot]),
    ("Battery voltage", &[Need::BatteryVoltage]),
    ("DoIP (diagnostics over Ethernet)", &[Need::Doip]),
];

/// `(use, allowed)` for each entry of [`USES`].
pub(crate) fn gated_uses(c: &Capabilities) -> Vec<(&'static str, bool)> {
    USES.iter()
        .map(|(l, needs)| (*l, caps::check(Some(c), needs).is_ok()))
        .collect()
}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

/// The Drivers view's state. The app hands it its adapter and jobs
/// ([`DriversView::show`]); without them the list still renders and the
/// probe says why it cannot run.
pub struct DriversView {
    driver: String,
    port: String,
    baud: String,
    bus: String,
    job: Option<JobHandle<ProbeReport>>,
    result: Option<(ProbeArgs, Result<ProbeReport, String>)>,
    pending: Option<ProbeArgs>,
    /// The port was filled from the app's link once.
    prefilled: bool,
}

impl Default for DriversView {
    fn default() -> Self {
        Self {
            driver: registered()
                .iter()
                .find(|(id, _)| *id == "elm327obd")
                .or(registered().first())
                .map(|(id, _)| id.to_string())
                .unwrap_or_default(),
            port: String::new(),
            baud: String::new(),
            bus: "hs".into(),
            job: None,
            result: None,
            pending: None,
            prefilled: false,
        }
    }
}

/// The app routes the Drivers view here until it keeps a [`DriversView`]
/// itself and calls [`DriversView::show`] with its adapter and jobs: the
/// state then lives in egui memory, and the probe is unavailable.
pub fn ui(ui: &mut Ui, t: &Tokens) {
    let id = egui::Id::unique("opentune-drivers-view");
    let state = ui.data_mut(|d| {
        d.get_temp_mut_or_insert_with(id, || Arc::new(Mutex::new(DriversView::default())))
            .clone()
    });
    let mut v = state.lock().unwrap_or_else(|p| p.into_inner());
    v.show(ui, t, None, None);
}

impl DriversView {
    /// Start the probe of the probe bar's request (refused when the request
    /// is invalid, a probe is running, or there is no adapter / job runner).
    pub(crate) fn start_probe(
        &mut self,
        adapter: Option<&Arc<dyn AdapterAccess>>,
        jobs: Option<&Jobs>,
    ) -> Result<(), String> {
        if self.job.is_some() {
            return Err("a probe is already running".into());
        }
        let args = probe_args(&self.driver, &self.port, &self.baud, &self.bus)?;
        let (Some(a), Some(jobs)) = (adapter, jobs) else {
            return Err(
                "probing needs the app's adapter link, which this view was not given".into(),
            );
        };
        let a = a.clone();
        let job_args = args.clone();
        self.job = Some(jobs.spawn("probe adapter", move |_| probe_op(&a, &job_args)));
        self.pending = Some(args);
        self.result = None;
        Ok(())
    }

    fn tick(&mut self) {
        let Some(r) = self.job.as_mut().and_then(|j| j.take()) else {
            return;
        };
        self.job = None;
        if let Some(args) = self.pending.take() {
            self.result = Some((args, r));
        }
    }

    /// Render the view: the probe bar (with `adapter` / `jobs`), then one
    /// card per registered driver.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        adapter: Option<&Arc<dyn AdapterAccess>>,
        jobs: Option<&Jobs>,
    ) {
        self.tick();
        if self.job.is_some() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
        }
        if !self.prefilled {
            self.prefilled = true;
            if let Some(l) = adapter.map(|a| a.link()) {
                if registered().iter().any(|(id, _)| *id == l.driver) {
                    self.driver = l.driver;
                    self.port = l.port;
                }
            }
        }
        let n = registered().len();
        let hw = hardware_count();
        let hidden = not_in_build().len();
        egui::Frame::new()
            .fill(t.surface)
            .inner_margin(Margin::symmetric(18, 11))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 11.0;
                    ui.label(RichText::new(icons::WRENCH).size(22.0).color(t.accent));
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(
                            RichText::new("Interface drivers")
                                .font(font(DISPLAY, 16.0))
                                .color(t.fg),
                        );
                        let mut line = format!(
                            "{n} drivers registered in this build · {hw} for hardware, {} \
                             offline simulators · status as each driver documents it",
                            n - hw
                        );
                        if hidden > 0 {
                            line.push_str(&format!(
                                " · {hidden} more (Bench tier) in alpha builds"
                            ));
                        }
                        ui.label(RichText::new(line).color(t.fg_dim));
                    });
                });
            });
        theme::hline(ui, t.border);
        egui::ScrollArea::vertical()
            .id_salt("drivers")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new()
                    .inner_margin(Margin::same(theme::GAP as i8))
                    .show(ui, |ui| {
                        self.probe_bar(ui, t, adapter, jobs);
                        ui.add_space(theme::GAP);
                        let gap = theme::GAP;
                        let avail = ui.available_width();
                        let cols = columns(avail, gap);
                        let w = ((avail - gap * (cols - 1) as f32) / cols as f32).floor();
                        let mut pick = None;
                        for row in registered().chunks(cols) {
                            ui.horizontal_top(|ui| {
                                ui.spacing_mut().item_spacing.x = gap;
                                for (id, name) in row {
                                    ui.allocate_ui_with_layout(
                                        Vec2::new(w, 0.0),
                                        Layout::top_down(Align::Min),
                                        |ui| {
                                            if card(ui, t, w, id, name, note_for(id)) {
                                                pick = Some(*id);
                                            }
                                        },
                                    );
                                }
                            });
                            ui.add_space(gap - ui.spacing().item_spacing.y);
                        }
                        if let Some(id) = pick {
                            self.driver = id.to_string();
                        }
                    });
            });
    }

    fn probe_bar(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        adapter: Option<&Arc<dyn AdapterAccess>>,
        jobs: Option<&Jobs>,
    ) {
        theme::panel_frame(t)
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icons::PLUGS_CONNECTED).size(18.0).color(t.accent));
                    theme::section_label(ui, t, "Probe an adapter");
                    ui.label(
                        RichText::new("a fresh handshake with the cable; nothing is sent to an ECU")
                            .small()
                            .color(t.fg_faint),
                    );
                });
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    let label = registered()
                        .iter()
                        .find(|(id, _)| *id == self.driver)
                        .map(|(_, n)| *n)
                        .unwrap_or("choose a driver");
                    egui::ComboBox::from_id_salt("drivers-probe-driver")
                        .selected_text(label)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (id, name) in registered() {
                                ui.selectable_value(&mut self.driver, id.to_string(), *name);
                            }
                        });
                    ui.label(RichText::new("port").color(t.fg_dim));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.port)
                            .hint_text("/dev/ttyUSB0, COM3, can0, host:port")
                            .desired_width(200.0),
                    );
                    ui.label(RichText::new("baud").color(t.fg_dim));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.baud)
                            .hint_text("default")
                            .desired_width(70.0),
                    );
                    ui.label(RichText::new("bus").color(t.fg_dim));
                    egui::ComboBox::from_id_salt("drivers-probe-bus")
                        .selected_text(match self.bus.as_str() {
                            "ms" => "MS-CAN",
                            "" => "–",
                            _ => "HS-CAN",
                        })
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.bus, "hs".into(), "HS-CAN");
                            ui.selectable_value(&mut self.bus, "ms".into(), "MS-CAN");
                        });
                    let can = adapter.is_some() && jobs.is_some() && self.job.is_none();
                    let r = ui.add_enabled_ui(can, |ui| {
                        theme::primary_button(ui, t, Some(icons::PLUG), "Probe")
                    });
                    if r.inner.clicked() {
                        if let Err(e) = self.start_probe(adapter, jobs) {
                            self.result = Some((
                                ProbeArgs {
                                    driver: self.driver.clone(),
                                    port: self.port.clone(),
                                    baud: 0,
                                    bus: self.bus.clone(),
                                },
                                Err(e),
                            ));
                        }
                    }
                    if self.job.is_some() {
                        ui.spinner();
                    }
                });
                if adapter.is_none() || jobs.is_none() {
                    line(
                        ui,
                        t,
                        Tone::Muted,
                        "Probing needs the app's adapter link; it is not available in this view yet.",
                    );
                }
                if let Some((args, r)) = &self.result {
                    ui.add_space(6.0);
                    report(ui, t, args, r);
                }
            });
    }
}

fn line(ui: &mut Ui, t: &Tokens, tone: Tone, s: &str) {
    let c = match tone {
        Tone::Ok => t.ok,
        Tone::Warn => t.warn,
        Tone::Crit => t.crit,
        Tone::Accent => t.accent,
        Tone::Muted => t.fg_dim,
    };
    ui.add(Label::new(RichText::new(s).font(font(UI_MEDIUM, 12.5)).color(c)).wrap());
}

fn report(ui: &mut Ui, t: &Tokens, args: &ProbeArgs, r: &Result<ProbeReport, String>) {
    let p = match r {
        Err(e) => return line(ui, t, Tone::Crit, e),
        Ok(p) => p,
    };
    if p.busy {
        return line(
            ui,
            t,
            Tone::Warn,
            &format!(
                "Adapter busy: {} — another operation holds it; try again when it ends.",
                p.error.as_deref().unwrap_or("")
            ),
        );
    }
    if !p.connected {
        return line(
            ui,
            t,
            Tone::Crit,
            &format!(
                "{}: no answer — {}",
                args.driver,
                p.error.as_deref().unwrap_or("not found")
            ),
        );
    }
    let mut head = format!("Found {}", p.name.as_deref().unwrap_or(&args.driver));
    if let Some(f) = p.firmware.as_deref().filter(|f| !f.is_empty()) {
        head.push_str(&format!(" · {f}"));
    }
    if let Some(s) = p.serial.as_deref().filter(|s| !s.is_empty()) {
        head.push_str(&format!(" · serial {s}"));
    }
    if let Some(mv) = p.battery_mv {
        head.push_str(&format!(" · battery {:.1} V", mv as f64 / 1000.0));
    }
    line(ui, t, Tone::Ok, &head);
    let Some(c) = &p.caps else { return };
    ui.label(
        RichText::new(format!("Reports: {}", caps::summary(Some(c))))
            .small()
            .color(t.fg_dim),
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(5.0, 5.0);
        for (label, ok) in gated_uses(c) {
            let (tone, mark) = if ok {
                (Tone::Ok, icons::CHECK)
            } else {
                (Tone::Muted, icons::X)
            };
            theme::pill(ui, t, &format!("{mark} {label}"), tone, false);
        }
    });
}

/// Card columns for a content width (cards are at least `MIN_CARD` wide).
const MIN_CARD: f32 = 300.0;
fn columns(width: f32, gap: f32) -> usize {
    (((width + gap) / (MIN_CARD + gap)).floor() as usize).clamp(1, 4)
}

/// One driver card; true when its "probe this" button was clicked.
fn card(ui: &mut Ui, t: &Tokens, w: f32, id: &str, name: &str, d: Option<&Driver>) -> bool {
    let mut pick = false;
    theme::panel_frame(t)
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(w - 28.0);
            ui.set_min_height(150.0);
            ui.spacing_mut().item_spacing.y = 6.0;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                let (r, _) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::hover());
                ui.painter().rect(
                    r,
                    CornerRadius::same(theme::RADIUS_SM),
                    t.surface3,
                    Stroke::new(1.0, t.border),
                    egui::StrokeKind::Inside,
                );
                ui.painter().text(
                    r.center(),
                    egui::Align2::CENTER_CENTER,
                    d.map_or(icons::PLUG, |d| d.icon),
                    egui::FontId::proportional(18.0),
                    t.accent,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        Label::new(
                            RichText::new(d.map_or(name, |d| d.name))
                                .font(font(DISPLAY_MEDIUM, 14.0))
                                .color(t.fg),
                        )
                        .truncate(),
                    )
                    .on_hover_text(name);
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("--driver {id}"))
                                .font(font(MONO_MEDIUM, 11.0))
                                .color(t.fg_faint),
                        );
                        if theme::icon_button(ui, t, icons::PLUGS_CONNECTED, "Select for the probe")
                            .clicked()
                        {
                            pick = true;
                        }
                    });
                });
            });
            let summary = d.map_or("No description for this driver yet.", |d| d.summary);
            ui.add(Label::new(RichText::new(summary).color(t.fg_dim)).wrap());
            let Some(d) = d else { return };
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(5.0, 5.0);
                if let Some(s) = d.status {
                    theme::pill(ui, t, s.label(), s.tone(), false);
                }
                for tr in d.transports {
                    theme::pill(ui, t, tr.label(), Tone::Accent, false);
                }
                for c in d.caps {
                    theme::pill(ui, t, c, Tone::Muted, true);
                }
            });
            if let Some(p) = d.platforms {
                ui.label(
                    RichText::new(format!("{}  {p}", icons::DESKTOP))
                        .small()
                        .color(t.fg_faint),
                );
            }
        });
    pick
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::{Ack, BackupReceipt, DiagJob, DiagTarget, LinkInfo};
    use std::any::Any;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn notes_are_unique_and_complete() {
        let ids: BTreeSet<_> = NOTES.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), NOTES.len(), "duplicate driver id");
        for d in NOTES {
            assert!(!d.transports.is_empty(), "{} has no transport", d.id);
            assert!(!d.caps.is_empty(), "{} has no capability", d.id);
            assert!(!d.summary.is_empty() && !d.name.is_empty(), "{}", d.id);
        }
    }

    /// The list is the registry: every registered driver has a note, and
    /// the Bench-tier drivers are listed exactly in alpha builds.
    #[test]
    fn the_list_follows_the_registry_and_the_build_tier() {
        let ids: Vec<&str> = registered().iter().map(|(id, _)| *id).collect();
        assert_eq!(count(), ids.len());
        assert!(ids.contains(&"mock") && ids.contains(&"vcar"));
        for id in &ids {
            assert!(note_for(id).is_some(), "{id} has no card note");
        }
        for bench in ["isotp-ble", "doip", "esp32-opentune"] {
            assert_eq!(ids.contains(&bench), cfg!(feature = "alpha"), "{bench}");
            assert_eq!(
                not_in_build().iter().any(|d| d.id == bench),
                !cfg!(feature = "alpha")
            );
        }
        let sims = ids
            .iter()
            .filter(|id| note_for(id).is_some_and(|d| d.transports.contains(&Simulator)))
            .count();
        assert_eq!(hardware_count(), ids.len() - sims);
    }

    #[test]
    fn column_count() {
        assert_eq!(columns(200.0, 14.0), 1);
        assert_eq!(columns(300.0, 14.0), 1);
        assert_eq!(columns(614.0, 14.0), 2);
        assert_eq!(columns(5000.0, 14.0), 4);
        assert_eq!(Status::AdapterChecked.tone(), Tone::Accent);
        assert_eq!(Status::Unverified.tone(), Tone::Warn);
    }

    #[test]
    fn probe_requests_are_checked() {
        let a = probe_args(" vcar ", " ", "", "HS").unwrap();
        assert_eq!(
            a,
            ProbeArgs {
                driver: "vcar".into(),
                port: String::new(),
                baud: 0,
                bus: "hs".into()
            }
        );
        assert_eq!(probe_args("mock", "", "38400", "").unwrap().baud, 38400);
        assert!(probe_args("no-such-driver", "", "", "").is_err());
        assert!(probe_args("mock", "", "fast", "").is_err());
        assert!(probe_args("mock", "", "0", "").is_err());
        assert!(probe_args("mock", "", "", "lin").is_err());
        if !cfg!(feature = "alpha") {
            // A Bench-tier driver is not registered, so it cannot be probed.
            assert!(probe_args("doip", "", "", "").is_err());
        }
    }

    #[test]
    fn probe_reports_parse_and_gate_the_panels() {
        let found = serde_json::json!({
            "connected": true, "driver": "vcar", "name": "Virtual car",
            "firmware": "sim 1", "serial": null, "battery_mv": 12600,
            "capabilities": {"can": true, "can_fd": false, "kline": false,
                "read_memory": false, "write_memory": false, "bench_boot": false,
                "battery_voltage": true, "programming_voltage": false, "doip": false}
        });
        let p = parse_probe(&found);
        assert!(p.connected && !p.busy);
        assert_eq!(p.battery_mv, Some(12600));
        let c = p.caps.unwrap();
        let uses = gated_uses(&c);
        let ok = |l: &str| uses.iter().find(|(x, _)| x.starts_with(l)).unwrap().1;
        assert!(ok("Diagnostics") && ok("CAN capture") && ok("Battery"));
        assert!(!ok("Flashing") && !ok("K-line") && !ok("Bench"));
        let busy = parse_probe(
            &serde_json::json!({"connected": false, "busy": true, "error": "adapter busy"}),
        );
        assert!(busy.busy && !busy.connected && busy.caps.is_none());
        let absent = parse_probe(&serde_json::json!({"connected": false, "error": "no port"}));
        assert_eq!(absent.error.as_deref(), Some("no port"));
    }

    /// An adapter that only answers probes (with what it was asked).
    struct ProbeOnly(Mutex<Vec<(String, String, u32, String)>>);

    impl AdapterAccess for ProbeOnly {
        fn link(&self) -> LinkInfo {
            LinkInfo {
                driver: "vcar".into(),
                port: "sim0".into(),
                ..Default::default()
            }
        }
        fn read_job(
            &self,
            _: &str,
            _: DiagTarget,
            _: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            Err("not used".into())
        }
        fn mutate_job(
            &self,
            _: &str,
            _: Ack,
            _: &BackupReceipt,
            _: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            Err("not used".into())
        }
        fn probe(
            &self,
            driver: &str,
            port: &str,
            baud: u32,
            bus: &str,
        ) -> Result<serde_json::Value, String> {
            self.0
                .lock()
                .unwrap()
                .push((driver.into(), port.into(), baud, bus.into()));
            Ok(
                serde_json::json!({"connected": true, "driver": driver, "name": "Synthetic",
                "capabilities": {"can": true}}),
            )
        }
    }

    #[test]
    fn the_probe_goes_through_the_adapter_and_reports() {
        let probe = Arc::new(ProbeOnly(Mutex::new(Vec::new())));
        let a: Arc<dyn AdapterAccess> = probe.clone();
        let jobs = Jobs::new(egui::Context::default());
        let mut v = DriversView::default();
        // No adapter: refused, nothing asked.
        assert!(v.start_probe(None, Some(&jobs)).is_err());
        v.driver = "no-such-driver".into();
        assert!(v.start_probe(Some(&a), Some(&jobs)).is_err());
        assert!(probe.0.lock().unwrap().is_empty());
        v.driver = "vcar".into();
        v.port = "sim0".into();
        v.baud = "500000".into();
        v.start_probe(Some(&a), Some(&jobs)).unwrap();
        assert!(
            v.start_probe(Some(&a), Some(&jobs)).is_err(),
            "one at a time"
        );
        for _ in 0..400 {
            v.tick();
            if v.job.is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let (args, r) = v.result.clone().expect("probe finished");
        assert_eq!(args.driver, "vcar");
        let r = r.unwrap();
        assert!(r.connected && r.caps.unwrap().can);
        assert_eq!(
            probe.0.lock().unwrap().as_slice(),
            [(
                "vcar".to_string(),
                "sim0".to_string(),
                500000,
                "hs".to_string()
            )]
        );
    }

    /// Every driver crate in `crates/drivers` has a note, and the ids match
    /// the registry list in docs/DRIVERS.md (the CLI's doc-drift test keeps
    /// that list in sync with `public_registry`).
    #[test]
    fn matches_the_source_tree() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let drivers = root.join("crates/drivers");
        let Ok(rd) = std::fs::read_dir(&drivers) else {
            return; // not built from a full checkout
        };
        let dirs: BTreeSet<String> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("Cargo.toml").is_file())
            .map(|e| format!("drivers/{}", e.file_name().to_string_lossy()))
            .collect();
        let ours: BTreeSet<String> = NOTES
            .iter()
            .map(|d| d.crate_dir.to_string())
            .filter(|c| c.starts_with("drivers/"))
            .collect();
        assert_eq!(dirs, ours, "driver crates vs notes");
        for d in NOTES {
            assert!(
                root.join("crates").join(d.crate_dir).is_dir(),
                "{}",
                d.crate_dir
            );
        }
        if let Ok(doc) = std::fs::read_to_string(root.join("docs/DRIVERS.md")) {
            for d in NOTES {
                assert!(
                    doc.contains(&format!("`{}`", d.id)),
                    "{} not in docs/DRIVERS.md",
                    d.id
                );
            }
        }
    }
}
