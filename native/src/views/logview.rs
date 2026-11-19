//! Datalog viewer: open a CSV log (OpenTune `log` / drive-logger output),
//! plot channels over time.

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;

#[derive(Default)]
pub struct LogView {}

impl LogView {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) {
        theme::empty_state(
            ui,
            t,
            icons::LIST_DASHES,
            "Log viewer",
            "Datalog viewer: being ported.",
        );
    }
}
