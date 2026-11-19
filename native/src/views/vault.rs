//! Flash backup vault: the full-image backups OpenTune saves before every
//! write (`opentune-vault/` beside the edited file).

use crate::icons;
use crate::theme::{self, Tokens};
use eframe::egui::Ui;
use std::path::Path;

#[derive(Default)]
pub struct VaultView {}

impl VaultView {
    /// `image` is the dump open in the Flash view (its vault is beside it).
    /// Returns a backup path the user chose to open in the Flash view.
    pub fn ui(
        &mut self,
        ui: &mut Ui,
        t: &Tokens,
        image: Option<&Path>,
    ) -> Option<std::path::PathBuf> {
        let _ = image;
        theme::empty_state(ui, t, icons::LOCK, "Vault", "Backup vault: being ported.");
        None
    }
}
