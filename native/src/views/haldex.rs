//! Haldex AWD: OpenHaldex, Haldex Gen4 over TP2.0, log decoding.
//!
//! Scaffold stub: batch N7 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(HaldexView)
}

pub struct HaldexView;

impl NativeView for HaldexView {
    fn id(&self) -> &str {
        "haldex"
    }
    fn label(&self) -> &str {
        "Haldex AWD"
    }
    fn icon(&self) -> &str {
        icons::GEAR_SIX
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
            "OpenHaldex, Haldex Gen4 over TP2.0 and log decoding.",
        );
    }
}
