//! Drive view: big glanceable tiles for live values or a replayed log.
//!
//! Scaffold stub: batch N4 replaces this file (and only this file). Keep
//! the module's `TIER` and `create()`: `views::builtin` registers the view
//! through them.

use crate::host::{Host, NativeView, Tier, ViewGroup};
use crate::icons;
use eframe::egui;

/// Registration tier, read before the view is built ([`Tier::Bench`]: only
/// alpha / bench builds register it).
pub const TIER: Tier = Tier::Public;

pub fn create() -> Box<dyn NativeView> {
    Box::new(DriveView)
}

pub struct DriveView;

impl NativeView for DriveView {
    fn id(&self) -> &str {
        "drive"
    }
    fn label(&self) -> &str {
        "Drive view"
    }
    fn icon(&self) -> &str {
        icons::CHART_LINE
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
            "Big glanceable tiles for live values (from the Dashboard / Live log feed, `Host::live_feed`) or a replayed log.",
        );
    }
}
