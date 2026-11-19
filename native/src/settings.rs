//! User settings, persisted with eframe's storage (window geometry is
//! persisted by eframe itself).

use crate::theme::ThemeMode;
use serde::{Deserialize, Serialize};

pub const STORAGE_KEY: &str = "opentune-native-settings";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Units {
    #[default]
    Metric,
    Imperial,
}

impl Units {
    pub fn label(self) -> &'static str {
        match self {
            Units::Metric => "Metric",
            Units::Imperial => "Imperial",
        }
    }
}

/// The adapter the user picked (the top bar's cable select).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdapterSettings {
    /// Driver id, e.g. "elm327-obd", "kkl", "socketcan" (empty = none).
    pub driver: String,
    /// Serial port / interface, e.g. "/dev/ttyUSB0", "can0".
    pub port: String,
    /// CAN bus of a dual-bus dongle: "hs" or "ms".
    pub can_bus: String,
    /// Request / response CAN ids (hex, blank = definition default).
    pub tx: String,
    pub rx: String,
}

/// Pane splits of the Flash dump view, as fractions of the available space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panes {
    /// Hex pane width / workspace width.
    pub hex: f32,
    /// Map pane height / right column height.
    pub map: f32,
}

impl Default for Panes {
    fn default() -> Self {
        Self {
            hex: 0.54,
            map: 0.58,
        }
    }
}

impl Panes {
    pub fn clamp(self) -> Self {
        Self {
            hex: self.hex.clamp(0.2, 0.8),
            map: self.map.clamp(0.15, 0.85),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeMode,
    pub units: Units,
    /// Bootstrap-loader baud for `bsl` reads (38400 works at 16/20/32 MHz).
    pub bsl_baud: u32,
    /// Send diagnostics (off; the public app has no upload target).
    pub telemetry: bool,
    pub adapter: AdapterSettings,
    pub panes: Panes,
    /// Last opened dump and definition, reopened on start.
    pub last_file: Option<String>,
    pub last_def: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::System,
            units: Units::Metric,
            bsl_baud: 38400,
            telemetry: false,
            adapter: AdapterSettings::default(),
            panes: Panes::default(),
            last_file: None,
            last_def: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_partial_json_load() {
        let s = Settings::default();
        assert_eq!(s.theme, ThemeMode::System);
        assert_eq!(s.bsl_baud, 38400);
        assert!(!s.telemetry);
        // Older/partial saved settings keep the other defaults.
        let p: Settings = serde_json::from_str(r#"{"bsl_baud": 57600}"#).unwrap();
        assert_eq!(p.bsl_baud, 57600);
        assert_eq!(p.panes, Panes::default());
        assert_eq!(
            Panes {
                hex: 0.99,
                map: 0.0
            }
            .clamp(),
            Panes {
                hex: 0.8,
                map: 0.15
            }
        );
    }
}
