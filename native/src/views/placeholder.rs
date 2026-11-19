//! Empty states for views that are not ported to the native app yet.

use crate::theme::{self, Tokens};
use crate::views::View;
use eframe::egui::Ui;

pub fn ui(ui: &mut Ui, t: &Tokens, view: View) {
    let text = match view {
        View::Project => "Tuning projects (.otproj): coming to the native app.",
        View::Vault => "The flash backup vault: coming to the native app.",
        View::Drive => "Live drive view over the selected adapter: coming to the native app.",
        View::Log => "Datalog viewer: coming to the native app.",
        View::Controllers => "Controller definitions: coming to the native app.",
        View::Sa2 => "SA2 script runner (your own scripts): coming to the native app.",
        _ => "Coming to the native app.",
    };
    theme::empty_state(ui, t, view.icon(), view.label(), text);
}
