//! The honest empty state of a view that is registered but not ported to
//! the native app yet (the scaffold stubs call it).

use crate::host::Host;
use crate::theme::{self, font, UI_MEDIUM, UI_STRONG};
use eframe::egui::{RichText, Ui};

/// `what` the view will do, and that it does nothing yet: it sends nothing
/// to an adapter and changes no file.
pub fn not_ported(ui: &mut Ui, host: &Host<'_>, icon: &str, label: &str, what: &str) {
    let t = *host.tokens();
    theme::empty_state(ui, &t, icon, label, what);
    ui.vertical_centered(|ui| {
        ui.add_space(12.0);
        ui.label(
            RichText::new("Not yet ported")
                .font(font(UI_STRONG, 13.0))
                .color(t.warn),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(
                "This panel is not in the native app yet: it does nothing and sends nothing. \
                 The webview app has it today.",
            )
            .font(font(UI_MEDIUM, 12.5))
            .color(t.fg_faint),
        );
    });
}
