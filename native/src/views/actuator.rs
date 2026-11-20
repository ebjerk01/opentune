//! Actuator tests: timed output tests with an automatic hand-back.
//!
//! Scaffold stub: batch N5 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(ActuatorView)
}

pub struct ActuatorView;

impl NativeView for ActuatorView {
    fn id(&self) -> &str {
        "actuator"
    }
    fn label(&self) -> &str {
        "Actuator tests"
    }
    fn icon(&self) -> &str {
        icons::POWER
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Vehicle
    }
    fn tier(&self) -> Tier {
        TIER
    }
    fn ui(&mut self, ui: &mut egui::Ui, host: &mut Host<'_>) {
        super::placeholder::not_ported(
            ui,
            host,
            self.icon(),
            self.label(),
            "Timed output tests, always handed back to the ECU afterwards.",
        );
    }
}
