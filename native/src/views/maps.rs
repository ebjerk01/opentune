//! Maps browser: every map of every ECU definition, searchable; picking one
//! opens it in the Flash dump view.

use crate::icons;
use crate::image::Image;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;
use opentune_core::ecudef::EcuDef;

#[derive(Default)]
pub struct MapsView {
    pub search: String,
}

impl MapsView {
    /// Returns `(definition index, map index)` when the user opens a map.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        defs: &[EcuDef],
        image: Option<&Image>,
    ) -> Option<(usize, usize)> {
        let _ = (defs, image);
        theme::empty_state(ui, t, icons::STACK, "Maps", "Map browser: being ported.");
        None
    }
}
