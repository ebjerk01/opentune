//! Guide: the bundled user guide and pinouts.
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
    Box::new(GuideView)
}

pub struct GuideView;

impl NativeView for GuideView {
    fn id(&self) -> &str {
        "guide"
    }
    fn label(&self) -> &str {
        "Guide"
    }
    fn icon(&self) -> &str {
        icons::BOOK_OPEN
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Help
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
            "The bundled user guide and pinouts.",
        );
    }
}
