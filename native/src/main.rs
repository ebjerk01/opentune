//! The public OpenTune native app. Embedding builds call
//! `opentune_native::run` with their own `NativeConfig`.

// A GUI program on Windows: no console window next to the app in release
// builds (output still reaches a caller that redirects stdout/stderr).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

fn main() -> eframe::Result {
    opentune_native::run(opentune_native::NativeConfig::default())
}
