//! Debug: raw requests, the offline UDS simulator, the session log.
//!
//! Scaffold stub: batch N8 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Bench;

pub fn create() -> Box<dyn NativeView> {
    Box::new(DebugView)
}

pub struct DebugView;

impl NativeView for DebugView {
    fn id(&self) -> &str {
        "debug"
    }
    fn label(&self) -> &str {
        "Debug"
    }
    fn icon(&self) -> &str {
        icons::BUG
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Bench
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
            "Raw requests, the offline UDS simulator and the session log.",
        );
    }
}
