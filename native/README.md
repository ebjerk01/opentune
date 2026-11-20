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

**Target module.** The app has one diagnostic target (Settings → Adapter
settings tx/rx, else the definition's ids, else the engine 7E0/7E8):
`Host::target()` and `Host::link().target`. Views pass it to
`AdapterAccess::read`. An ECU-changing operation is confirmed with
`Host::confirm_ecu`, whose dialog shows the target and whose `Ack` carries
it; `AdapterAccess::mutate` addresses `Ack::ecu_target()`, so a write reaches
exactly the module the user confirmed, and a plain `confirm` Ack (file
writes) cannot drive a mutation.

**Diagnostics.** `NativeConfig::diagnostics` is an optional upload seam
(`DiagnosticsSink`); it is `None` in the public build, which never uploads
anything.

A view edits map cells with `HostAction::EditCells { map, cells }` (physical
values, applied to the working image; `Host::image()` shows them and
`Host::image_edited()` says there are unsaved edits). Views never save the
dump themselves: the user saves from the Flash view, which runs the same
confirm → vault backup → atomic replace sequence and corrects checksums with
the configured resolvers.

## Safety

- The core views never write to an ECU. Auto-detect only sends ELM327 AT
  commands (`ATI`) to ELM-looking ports and never opens K-line, CAN or
  unknown ports. A connected cable (any registry driver) is owned by one
  `opentune-hub` session: the top bar's link only opens the driver (its
  adapter set-up) and reads the battery voltage, and views run their
  diagnostics through the same session (`Host::adapter()`), one operation
  at a time. A view's ECU write needs `Host::confirm_ecu` + a vault backup
  and goes to the module named in that confirmation.
- "Fix & save" (no edits) writes corrected checksums to a NEW file chosen
  in a save dialog; the opened dump is never overwritten by it.
- Map editing: click a cell, type a value + Enter (or `+`/`-` to step one
  raw LSB, Shift for 10, Ctrl+Z to undo). Values are refused, never clamped
  or wrapped, when they do not fit the data type or the map's `min`/`max`.
  Edited cells are marked; the dump bar shows the edited byte count.
- **Save** replaces the opened dump only after a confirmation; its current
  contents are archived in the vault first (the write is refused if the
  backup fails) and the new file is written atomically. Checksums are
  corrected first when the selected algorithm *recognises* the image and
  must verify afterwards, else nothing is written; with no recognising
  algorithm the dialog says checksums are NOT corrected. **Save as…**
  writes a new file (replacing an existing one goes through the same
  confirm + backup). Opening another dump, reverting or quitting with
  unsaved edits asks first.
