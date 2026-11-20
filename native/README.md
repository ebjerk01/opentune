# opentune-native

The native OpenTune desktop app: **egui + eframe on wgpu** (Vulkan / Metal /
DX12 / GL), no webview on any OS. It calls the Rust core **in-process**
(`opentune-core`, `-checksum`, `-scan`, `-db`); there is no IPC bridge.

Status: **in development** on its own line (roadmap APP-13, planned as
1.0.0-beta). The Tauri app in `desktop/` is the shipping app and is not
affected by this crate.

## Layout

```
src/
  main.rs        arguments, window options
  app.rs         shell: global top bar, sidebar, status bar, view routing,
                 drag-and-drop / Ctrl+O, settings persistence
  theme.rs       design tokens (light/dark), bundled fonts, egui style,
                 widgets: panel, buttons, pills, stats, empty states
  icons.rs       Phosphor glyph constants (generated)
  settings.rs    persisted settings (theme, units, BSL baud, adapter, panes)
  adapters.rs    serial / SocketCAN discovery, ELM327 auto-detect and link
  shell/         top bar (cable select, auto-detect, battery, link, settings
                 menu) and sidebar
  views/         flash (hex | map / checksum), maps, ecudb, drivers, vault,
                 project, logview, controllers, placeholder
  hex.rs         virtualised hex rows
  mapview.rs     heat-grid map canvas + legend
  image.rs       map decoding (thin local copy of the Tauri backend's)
  bench.rs       --bench frame timing
  screenshot.rs  --screenshot capture
assets/fonts/    Inter, Space Grotesk, JetBrains Mono (SIL OFL 1.1, subset,
                 see README there), Phosphor (MIT)
```

The map decoding is a thin copy of what `desktop/src-tauri/src/maps.rs`
does; the plan is to move the shared logic into a core crate both frontends
use.

## Build and run

It is not part of the root workspace (own `[workspace]` and lockfile):

```sh
cd native
cargo run --release -- [path/to/dump.bin] [--def bosch-edc15c6]
```

ECU definitions are read from `ecudefs/`, `../ecudefs/` or next to the
executable (or `--defs DIR`); the control-unit database from `database/`.
On Linux it runs on Wayland or X11 (unset `WAYLAND_DISPLAY` to force X11).
Settings and the window geometry persist between runs; the last dump and
definition reopen.

## Review and measurement modes

```sh
# Capture the window to a PNG after it settles, then exit (settings untouched).
cargo run --release -- dump.bin --theme dark --view flash --screenshot out.png
# Scripted scroll with per-frame timing (fps, p50/p95/max, frames over
# 16.7 / 25 / 50 ms, UI CPU time), summary on stdout, every frame in the CSV.
cargo run --release -- dump.bin --bench hex --frames 600 --px 24 --csv hex.csv [--no-vsync]
```

Views for `--view`: flash, maps, ecudb, drivers, vault, project, log,
controllers, sa2, drive.

## Embedding (plugin host)

The crate is a library (`opentune_native`) plus a thin binary. Another build
embeds it with its own configuration:

```rust
use opentune_native::{host::*, NativeConfig};
fn main() -> eframe::Result {
    opentune_native::run(NativeConfig {
        branding: Branding { name: "My build".into(), pill: String::new(), app_id: "my-build".into() },
        checksums: vec![my_resolver],          // consulted before the core algorithms
        views: vec![Box::new(MyView::default())], // extra sidebar destinations
        adapter: None,                         // or Some(Arc<dyn AdapterAccess>)
    })
}
```

A view implements `host::NativeView` (`id`, `label`, `icon`, `group`,
`count`, `ui(ui, &mut Host)`). `Host` gives read access to the open image,
the definitions and the selection, theme tokens, background jobs
(`spawn_job` with progress + cancel), file dialogs, a status line, actions
(open image, select map, hex jump), backups to the vault, and the adapter.

The host enforces the safety rules: `write_file` needs an `Ack` (only
`confirm` creates one) and archives the old contents to the vault first;
`AdapterAccess::mutate` needs an `Ack` and a `BackupReceipt` (only `backup`
creates one). Neither type can be constructed outside this crate.

## Safety

- Nothing here writes to an ECU. The adapter layer only sends ELM327 AT
  commands (identification, battery voltage) and never opens K-line, CAN or
  unknown ports during auto-detect.
- "Fix & save" writes corrected checksums to a NEW file chosen in a save
  dialog; the opened dump is never overwritten.
