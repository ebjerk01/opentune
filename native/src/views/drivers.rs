//! Interface drivers OpenTune supports, with what each can do.

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;

pub fn ui(ui: &mut Ui, t: &Tokens) {
    theme::empty_state(
        ui,
        t,
        icons::WRENCH,
        "Drivers",
        "Driver list: being ported.",
    );
}
