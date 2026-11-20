//! SA2 script: load, disassemble and preview your own SA2 scripts.
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
    Box::new(Sa2View)
}

pub struct Sa2View;

impl NativeView for Sa2View {
    fn id(&self) -> &str {
        "sa2"
    }
    fn label(&self) -> &str {
        "SA2 script"
    }
    fn icon(&self) -> &str {
        icons::CODE
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
            "Load, disassemble and preview your own SA2 seed/key scripts.",
        );
    }
}
