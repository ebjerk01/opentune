//! Adaptation: UDS / KWP2000 adaptation channels.
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
    Box::new(AdaptationView)
}

pub struct AdaptationView;

impl NativeView for AdaptationView {
    fn id(&self) -> &str {
        "adaptation"
    }
    fn label(&self) -> &str {
        "Adaptation"
    }
    fn icon(&self) -> &str {
        icons::ARROWS_CLOCKWISE
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
            "UDS / KWP2000 adaptation channels: read, basic settings, adapt.",
        );
    }
}
