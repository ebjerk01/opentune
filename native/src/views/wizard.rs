//! Check my car: a guided health check.
//!
//! Scaffold stub: batch N9 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(WizardView)
}

pub struct WizardView;

impl NativeView for WizardView {
    fn id(&self) -> &str {
        "wizard"
    }
    fn label(&self) -> &str {
        "Check my car"
    }
    fn icon(&self) -> &str {
        icons::CAR
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
            "A guided health check of the car.",
        );
    }
}
