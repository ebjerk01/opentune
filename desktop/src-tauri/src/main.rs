// OpenTune desktop — the public app binary. All of the backend lives in the
// `opentune_desktop` library (src/lib.rs) so other Tauri apps can embed it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    opentune_desktop::run(tauri::generate_context!());
}
