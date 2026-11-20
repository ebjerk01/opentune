//! Utilities: VIN decoder, part number and fault code lookup, seed/key calculator.
//!
//! Scaffold stub: batch N3 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(UtilitiesView)
}

pub struct UtilitiesView;

impl NativeView for UtilitiesView {
    fn id(&self) -> &str {
        "utilities"
    }
    fn label(&self) -> &str {
        "Utilities"
    }
    fn icon(&self) -> &str {
        icons::TOOLBOX
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Reference
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
            "VIN decoder, part number and fault code lookup, and a seed/key calculator.",
        );
    }
}
