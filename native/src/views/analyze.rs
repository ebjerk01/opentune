//! Analyze bin: the full offline report of the opened dump.
//!
//! Scaffold stub: batch N1 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(AnalyzeView)
}

pub struct AnalyzeView;

impl NativeView for AnalyzeView {
    fn id(&self) -> &str {
        "analyze"
    }
    fn label(&self) -> &str {
        "Analyze bin"
    }
    fn icon(&self) -> &str {
        icons::MAGNIFYING_GLASS
    }
    fn group(&self) -> ViewGroup {
        ViewGroup::Calibration
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
            "The full offline report of the opened dump: family, hardware and software numbers, checksum candidates and matching vehicle catalog entries.",
        );
    }
}
