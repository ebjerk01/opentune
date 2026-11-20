//! The optional GPS source and its card (Live log / Dashboard).
//!
//! One app-wide source ([`GpsSource`], shared through `Host::gps`): a u-blox
//! style receiver on a serial / USB CDC-ACM port read by a background thread,
//! or a recorded NMEA file replayed (looping) for demos and offline checks.
//! It never touches the adapter hub: the receiver is its own port. The
//! panels that host the card are Bench tier until vehicle-tested.

use crate::host::Host;
use crate::icons;
use crate::theme::{self, font, MONO_MEDIUM, UI_STRONG};
use eframe::egui::{self, RichText, Ui};
use opentune_gps::{GpsFeed, GpsReader, GpsSnapshot, PortInfo, ReplayFeed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How often the card polls the source (a replay advances one epoch per
/// poll, so this is also the replay rate).
pub const POLL: Duration = Duration::from_millis(500);

struct Active {
    feed: Box<dyn GpsFeed>,
    source: String,
    kind: &'static str,
    last: Option<(Instant, GpsSnapshot)>,
}

/// The app-wide GPS source. Cheap to clone; clones share it.
#[derive(Clone, Default)]
pub struct GpsSource(Arc<Mutex<Option<Active>>>);

impl GpsSource {
    fn slot(&self) -> MutexGuard<'_, Option<Active>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open `spec`: an existing file is replayed (looping), anything else is
    /// opened as a serial port at `baud` (CDC-ACM ignores it). Replaces any
    /// open source. Returns the source's description.
    pub fn open(&self, spec: &str, baud: u32) -> Result<String, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("choose a GPS port or an NMEA file".into());
        }
        // Close the old source first: it may hold the same port.
        *self.slot() = None;
        let path = std::path::Path::new(spec);
        let (feed, kind): (Box<dyn GpsFeed>, &'static str) = if path.is_file() {
            let r = ReplayFeed::open(path)
                .map_err(|e| format!("read NMEA file '{spec}': {e}"))?
                .looping(true);
            (Box::new(r), "replay")
        } else {
            (Box::new(GpsReader::open_serial(spec, baud)?), "serial")
        };
        Ok(self.open_feed(feed, spec, kind))
    }

    /// Use `feed` as the source (e.g. an in-memory replay).
    pub fn open_feed(&self, feed: Box<dyn GpsFeed>, source: &str, kind: &'static str) -> String {
        let d = feed.describe();
        *self.slot() = Some(Active {
            feed,
            source: source.into(),
            kind,
            last: None,
        });
        d
    }

    /// Stop the source; `true` if one was open.
    pub fn close(&self) -> bool {
        self.slot().take().is_some()
    }

    pub fn is_open(&self) -> bool {
        self.slot().is_some()
    }

    /// `(source, "serial" | "replay")` of the open source.
    pub fn source(&self) -> Option<(String, &'static str)> {
        self.slot().as_ref().map(|a| (a.source.clone(), a.kind))
    }

    /// The latest snapshot; the feed is polled at most every [`POLL`].
    pub fn snapshot(&self) -> Option<GpsSnapshot> {
        let mut g = self.slot();
        let a = g.as_mut()?;
        match &a.last {
            Some((at, s)) if at.elapsed() < POLL => Some(s.clone()),
            _ => {
                let s = a.feed.snapshot();
                a.last = Some((Instant::now(), s.clone()));
                Some(s)
            }
        }
    }
}

/// The GPS card: pick a port or an NMEA file, open / close, and the fix.
#[derive(Default)]
pub struct GpsCard {
    spec: String,
    ports: Vec<PortInfo>,
    note: Option<String>,
}

impl GpsCard {
    pub fn ui(&mut self, ui: &mut Ui, host: &Host<'_>) {
        let t = *host.tokens();
        let gps = host.gps();
        theme::panel_frame(&t).show(ui, |ui| {
            ui.label(
                RichText::new(format!("{}  GPS", icons::GPS))
                    .font(font(UI_STRONG, 13.5))
                    .color(t.fg),
            );
            ui.add_space(4.0);
            match gps.source() {
                Some((src, kind)) => {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{kind}: {src}")).color(t.fg_dim));
                        if theme::button(ui, &t, Some(icons::X), "Close").clicked() {
                            gps.close();
                        }
                    });
                    if let Some(s) = gps.snapshot() {
                        fix_rows(ui, &t, &s);
                    }
                    ui.ctx().request_repaint_after(POLL);
                }
                None => {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.spec)
                                .hint_text("/dev/ttyACM0 · COM5 · NMEA file")
                                .desired_width(200.0),
                        );
                        ui.menu_button(icons::LIST, |ui| {
                            if self.ports.is_empty() {
                                self.ports = opentune_gps::list_ports();
                            }
                            if self.ports.is_empty() {
                                ui.label("No serial ports");
                            }
                            for p in &self.ports {
                                let star = if p.likely_gnss { " (GNSS)" } else { "" };
                                if ui.button(format!("{}{star}", p.name)).clicked() {
                                    self.spec = p.name.clone();
                                }
                            }
                        });
                        if theme::button(ui, &t, Some(icons::FILE), "NMEA file…").clicked() {
                            if let Some(p) = host.pick_file(
                                "Replay an NMEA file",
                                &[("NMEA", &["nmea", "txt", "log"])],
                            ) {
                                self.spec = p.display().to_string();
                            }
                        }
                        if theme::primary_button(ui, &t, None, "Open").clicked() {
                            self.note = gps.open(&self.spec, opentune_gps::DEFAULT_BAUD).err();
                        }
                    });
                    if let Some(n) = &self.note {
                        ui.label(RichText::new(n).color(t.crit));
                    }
                }
            }
        });
    }
}

fn fix_rows(ui: &mut Ui, t: &theme::Tokens, s: &GpsSnapshot) {
    let f = &s.fix;
    let (state, col) = if s.good {
        ("fix", t.ok)
    } else if s.stale {
        ("stale", t.warn)
    } else {
        ("no fix", t.fg_dim)
    };
    let opt = |v: Option<f64>, d: usize| v.map_or("–".to_string(), |v| format!("{v:.d$}"));
    egui::Grid::new("gps-fix").num_columns(2).show(ui, |ui| {
        let mut row = |k: &str, v: String, c| {
            ui.label(RichText::new(k).color(t.fg_dim));
            ui.label(RichText::new(v).font(font(MONO_MEDIUM, 12.5)).color(c));
            ui.end_row();
        };
        row("State", state.into(), col);
        row("Latitude", opt(f.lat, 6), t.fg);
        row("Longitude", opt(f.lon, 6), t.fg);
        row("Speed km/h", opt(f.speed_kmh, 1), t.fg);
        row("Altitude m", opt(f.alt_m, 1), t.fg);
        row(
            "Satellites",
            f.sats.map_or("–".into(), |n| n.to_string()),
            t.fg,
        );
        row("HDOP", opt(f.hdop, 1), t.fg);
    });
    if !s.note.is_empty() {
        ui.label(RichText::new(&s.note).color(t.fg_faint));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NMEA: &str = "$GPRMC,123519,A,4807.038,N,01131.000,E,022.4,084.4,230394,003.1,W*6A\r\n\
$GPGGA,123519,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*47\r\n";

    #[test]
    fn one_shared_source_polled_at_most_every_interval() {
        let a = GpsSource::default();
        let b = a.clone();
        assert!(a.snapshot().is_none() && !b.is_open());
        let feed = ReplayFeed::from_bytes("test", NMEA.as_bytes()).looping(true);
        a.open_feed(Box::new(feed), "mem", "replay");
        assert_eq!(b.source(), Some(("mem".to_string(), "replay")));
        let s1 = b.snapshot().unwrap();
        let s2 = a.snapshot().unwrap();
        assert_eq!(s1, s2, "cached within the poll interval");
        assert!(s1.fix.lat.is_some(), "{s1:?}");
        assert!(b.close());
        assert!(!a.is_open() && !a.close());
        assert!(a.open("  ", 9600).unwrap_err().contains("choose"));
    }
}
