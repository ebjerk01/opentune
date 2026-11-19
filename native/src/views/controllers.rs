//! Controller definitions (ctrldefs/ via opentune-ctrldef).

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;

#[derive(Default)]
pub struct ControllersView {}

impl ControllersView {
    pub fn ui(&mut self, ui: &mut Ui, t: &Tokens) {
        theme::empty_state(
            ui,
            t,
            icons::CIRCUITRY,
            "Controllers",
            "Controller definitions: being ported.",
        );
    }
}
