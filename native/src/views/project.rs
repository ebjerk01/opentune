//! Tuning projects (.otproj): a dump, its definition and notes, saved and
//! reopened together.

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;
use std::path::{Path, PathBuf};

/// What the Project view asks the app to do.
pub enum ProjectAction {
    /// Open this dump with this definition id in the Flash view.
    Open {
        image: PathBuf,
        def_id: Option<String>,
    },
}

#[derive(Default)]
pub struct ProjectView {}

impl ProjectView {
    /// `image` / `def_id`: what the Flash view has open now (to save).
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        image: Option<&Path>,
        def_id: Option<&str>,
    ) -> Option<ProjectAction> {
        let _ = (image, def_id);
        theme::empty_state(
            ui,
            t,
            icons::KANBAN,
            "Project",
            "Tuning projects: being ported.",
        );
        None
    }
}
