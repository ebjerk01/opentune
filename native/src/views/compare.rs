//! Compare: two dumps map by map, and the stock / tuned map finder.
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
    Box::new(CompareView)
}

pub struct CompareView;

impl NativeView for CompareView {
    fn id(&self) -> &str {
        "compare"
    }
    fn label(&self) -> &str {
        "Compare"
    }
    fn icon(&self) -> &str {
        icons::GIT_DIFF
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
            "Two dumps compared map by map, and the stock / tuned map finder.",
        );
    }
}
