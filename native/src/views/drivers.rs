//! Interface drivers OpenTune supports, with what each can do.
//!
//! The table below is written from each driver crate's own documentation
//! (`crates/drivers/*/src/lib.rs`, `docs/DRIVERS.md`) and its declared
//! `Capabilities`. A validation state is only shown where the driver's source
//! states one; nothing here claims hardware verification the source does not.

use crate::icons;
use crate::theme::{self, font, Tokens, Tone, DISPLAY, DISPLAY_MEDIUM, MONO_MEDIUM};
use eframe::egui::{self, Align, CornerRadius, Label, Layout, Margin, RichText, Stroke, Ui, Vec2};

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

const DRIVERS: &[Driver] = &[
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

/// Card columns for a content width (cards are at least `MIN_CARD` wide).
/// Number of driver cards (sidebar count).
pub fn count() -> usize {
    DRIVERS.len()
}

const MIN_CARD: f32 = 300.0;
fn columns(width: f32, gap: f32) -> usize {
    (((width + gap) / (MIN_CARD + gap)).floor() as usize).clamp(1, 4)
}

/// Number of drivers that touch real hardware (everything but simulators).
fn hardware_count() -> usize {
    DRIVERS
        .iter()
        .filter(|d| !d.transports.contains(&Simulator))
        .count()
}

pub fn ui(ui: &mut Ui, t: &Tokens) {
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
                    ui.label(
                        RichText::new(format!(
                            "{} drivers in the public build · {} for hardware, {} offline \
                             simulators · status as each driver documents it",
                            DRIVERS.len(),
                            hardware_count(),
                            DRIVERS.len() - hardware_count()
                        ))
                        .color(t.fg_dim),
                    );
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
                    let gap = theme::GAP;
                    let avail = ui.available_width();
                    let cols = columns(avail, gap);
                    let w = ((avail - gap * (cols - 1) as f32) / cols as f32).floor();
                    for row in DRIVERS.chunks(cols) {
                        ui.horizontal_top(|ui| {
                            ui.spacing_mut().item_spacing.x = gap;
                            for d in row {
                                ui.allocate_ui_with_layout(
                                    Vec2::new(w, 0.0),
                                    Layout::top_down(Align::Min),
                                    |ui| card(ui, t, w, d),
                                );
                            }
                        });
                        ui.add_space(gap - ui.spacing().item_spacing.y);
                    }
                });
        });
}

fn card(ui: &mut Ui, t: &Tokens, w: f32, d: &Driver) {
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
                    d.icon,
                    egui::FontId::proportional(18.0),
                    t.accent,
                );
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.add(
                        Label::new(
                            RichText::new(d.name)
                                .font(font(DISPLAY_MEDIUM, 14.0))
                                .color(t.fg),
                        )
                        .truncate(),
                    );
                    ui.label(
                        RichText::new(format!("--driver {}", d.id))
                            .font(font(MONO_MEDIUM, 11.0))
                            .color(t.fg_faint),
                    );
                });
            });
            ui.add(Label::new(RichText::new(d.summary).color(t.fg_dim)).wrap());
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn ids_unique_and_cards_complete() {
        let ids: BTreeSet<_> = DRIVERS.iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), DRIVERS.len(), "duplicate driver id");
        for d in DRIVERS {
            assert!(!d.transports.is_empty(), "{} has no transport", d.id);
            assert!(!d.caps.is_empty(), "{} has no capability", d.id);
            assert!(!d.summary.is_empty() && !d.name.is_empty(), "{}", d.id);
        }
        assert_eq!(hardware_count(), DRIVERS.len() - 3);
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

    /// Every driver crate in `crates/drivers` has a card, and the ids match
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
        let ours: BTreeSet<String> = DRIVERS
            .iter()
            .map(|d| d.crate_dir.to_string())
            .filter(|c| c.starts_with("drivers/"))
            .collect();
        assert_eq!(dirs, ours, "driver crates vs cards");
        for d in DRIVERS {
            assert!(
                root.join("crates").join(d.crate_dir).is_dir(),
                "{}",
                d.crate_dir
            );
        }
        if let Ok(doc) = std::fs::read_to_string(root.join("docs/DRIVERS.md")) {
            for d in DRIVERS {
                assert!(
                    doc.contains(&format!("`{}`", d.id)),
                    "{} not in docs/DRIVERS.md",
                    d.id
                );
            }
        }
    }
}
