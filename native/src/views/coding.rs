//! Coding: read, preview and write module coding.
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
    Box::new(CodingView)
}

pub struct CodingView;

impl NativeView for CodingView {
    fn id(&self) -> &str {
        "coding"
    }
    fn label(&self) -> &str {
        "Coding"
    }
    fn icon(&self) -> &str {
        icons::SLIDERS_HORIZONTAL
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
            "Read, preview and write module coding.",
        );
    }
}
