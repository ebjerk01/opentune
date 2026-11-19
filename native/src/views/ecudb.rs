//! ECU database browser (database/units via opentune-db): units, pinouts,
//! access profiles.

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;

#[derive(Default)]
pub struct EcuDbView {}

impl EcuDbView {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) {
        theme::empty_state(
            ui,
            t,
            icons::CPU,
            "ECU database",
            "Database browser: being ported.",
        );
    }

    /// Number of units, once loaded (sidebar count).
    pub fn count(&self) -> Option<usize> {
        None
    }
}
