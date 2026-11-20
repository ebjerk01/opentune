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
  host.rs        the view API (`NativeView`, `Host`, `AdapterAccess`)
  hublink.rs     the built-in adapter over one `opentune-hub` session
  ops.rs         actuate / long-running / scan / raw operation shapes
  livefeed.rs    Dashboard / Live log → Drive view live feed
  shell/         top bar (cable select over the full driver registry,
                 auto-detect, battery, link + capabilities, settings menu)
                 and sidebar
  views/         core views: flash (hex | map / checksum), maps, ecudb,
                 drivers, vault, project, logview, controllers;
                 built-in NativeViews registered in builtin.rs (one module
                 each; not yet ported ones are stubs); placeholder
  widgets/       shared panel widgets: pack picker, GPS card, capability
                 gating, CSV recorder
  hex.rs         virtualised hex rows
  mapview.rs     heat-grid map canvas + legend
  image.rs       map decoding (thin local copy of the Tauri backend's)
  bench.rs       --bench frame timing
  screenshot.rs  --screenshot capture
assets/fonts/    Inter, Space Grotesk, JetBrains Mono (SIL OFL 1.1, subset,
                 see README there), Phosphor (MIT)
```

The map decoding (`image.rs`) is a local copy that predates the shared
codec in `opentune_hub::maps` (which the Tauri app uses); switching the
native app to it is planned.

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
controllers, or any built-in / plugin view id (drive, sa2, analyze, ...).

## Build channel

`cargo build --features alpha` builds the alpha channel: the hub registers
its Bench-tier drivers (isotp-ble, doip, esp32) and the Bench-tier views and
raw adapter access are enabled. On the default (stable) channel a Bench view
is never registered (no sidebar entry, no `ui()`), and `AdapterAccess::raw`
is refused in the adapter itself, not only hidden in the UI. An embedding
build sets `NativeConfig::channel` (e.g. `bench`).

## Built-in views

`views/builtin.rs` lists every built-in `NativeView` as `(TIER, create)`;
each lives in its own module that exports `TIER` and `create()`. The app
renders them through the same `Host` as plugin views. A feature batch
replaces only its own module (the scaffold stub) and keeps those two names;
it never edits `builtin.rs`, `views/mod.rs` or `app.rs`. The sidebar groups
are Calibration, Vehicle, Reference, Help, then Bench (every `Tier::Bench`
view); the order inside a group is `views::NAV_ORDER`. A view graduates from
Bench by changing its module's `TIER`.

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
        channel: BuildChannel::new(BuildChannel::BENCH), // registers Tier::Bench views
        ..Default::default()                   // seedkeys, seedkey_loader, diagnostics
    })
}
```

A view implements `host::NativeView` (`id`, `label`, `icon`, `group`,
`tier`, `count`, `ui(ui, &mut Host)`). `Host` gives read access to the open
image, the definitions and the selection, theme tokens, background jobs
(`spawn_job` with progress + cancel), file dialogs, a status line, actions
(open image, select map, hex jump, edit cells, add a definition), backups
to the vault, the adapter and its capabilities, the build channel
(`channel`, `bench_enabled`), the bundled data folders (`data_dir`), the
live feed (`live_feed`) and the shared GPS source (`gps`).

**Seed/key definitions loader.** `NativeConfig::seedkey_loader` takes the
user's UnlockECU-format definitions (as the Tauri app's
`DesktopConfig::seedkey_loader`): `None` in the public build, honoured only
on alpha / bench channels.

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

**Protocol clients.** Inside an adapter job, wrap the `RawDiag` in
`RawTransport::new(d)` to use the core `UdsClient` / `Kwp2000Client`. The
responsePending (0x78) wait is done once by the driver's transport, which
never re-sends a request.

**Security access.** `NativeConfig::seedkeys` takes seed/key resolvers
(empty in the public build: OpenTune ships no algorithm). The ECU
definition's `seedkey` field names the id; `Host::unlocker()` resolves it
(nothing is sent if it cannot), and inside the adapter job
`unlocker.unlock_uds(d, level)` / `unlock_kwp(d, level)` runs
`27 level` → key → `27 level+1`. The level (odd) is the operation's, so
the view passes it. A rejected key is never retried; an all-zero seed means
already unlocked.

**Block download.** `transfer::download(d, &Download::new(addr, &data),
&progress)` runs `34` → `36` blocks (ECU-sized, counter wrap, every echo
checked, nothing re-sent) → `37`. It only runs inside `adapter.mutate(…)`
(it refuses, sending nothing, anywhere else); the caller drives session,
security access and routines around it. Cancel is honoured before the start
and between blocks only, and `TransferError` says what the ECU was left with
(not started / PARTIALLY WRITTEN / exit failed).

**Non-persistent actions (`actuate`).** An output test, a timed routine or
a mode toggle runs as `adapter.actuate(op, ack, run, restore)`: the Ack of
`Host::confirm_ecu` (it addresses `Ack::ecu_target()`), no backup.
`restore` (return control to the ECU) ALWAYS runs after `run`, on the same
session, even when `run` fails or panics; the returned `Actuation` says
whether the hand-back succeeded. It is not a mutation: `transfer::download`
refuses inside it. Persistent changes (coding, adaptation values, flash)
stay on `mutate` (read the old value → `Host::backup` → write).

**Per-op table (required).** Every panel that changes an ECU keeps an
explicit, reviewable table of its operations, each classified as `mutate`
(persistent) or `actuate` (non-persistent). The class is written down per
op, never inferred from service ids or request bytes.

**Long-running operations.** `adapter.start(op, target, |d, stop| …)`
returns an `OpHandle` (`stop`, `poll`, `take`, `join`); the operation holds
the adapter until its job returns and must end when `stop` is set.
Dropping the handle stops it.

**Multi-module scans.** `adapter.scan(op, |m| …)` is one READ-ONLY
operation that re-binds per module with `m.bind(target)`; ECU-changing
requests (`ops::ECU_CHANGING_SERVICES`, e.g. `14`, `2E`, `31`, `34`/`36`)
are refused unsent. Clearing DTCs is a separate confirmed operation.

**Raw driver access (Bench tier).** `adapter.raw(op, RawBinding, |i| …)`
hands the job a `RawInterface` (CAN monitor, raw K-line, a TP2.0 binding
via driver options). It is low level and Bench only: refused in the
adapter on stable channels. The wrapper refuses `write_memory`,
`set_seed_key`, `open` / `close`, and ECU-changing requests through
`diag()`, so raw access cannot bypass `mutate`; CAN frames and raw K-line
bytes are not inspected. The driver is re-opened after every raw operation.

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
  and goes to the module named in that confirmation; a non-persistent
  action needs the same confirmation and always hands control back.
- Bench-tier views and raw adapter access do not exist on the stable
  channel (decided at registration and in the adapter, not by hiding).
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
