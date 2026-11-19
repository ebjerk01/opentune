# opentune-native (prototype)

The native OpenTune frontend: **egui + eframe on wgpu** (Vulkan / Metal /
DX12 / GL), no webview on any OS. It calls the Rust core **in-process**
(`opentune-core`, `-checksum`, `-scan`); there is no IPC bridge.

Status: **proof-of-approach** (roadmap APP-13). The Tauri app in `desktop/`
is the shipping app and is not affected by this crate.

## What it does today

- Open a flash image (file dialog or command line); identify it in-process
  with the same scanner the CLI and the webview app use; pick the matching
  ECU definition.
- **Hex view**, virtualised: only the visible rows are laid out, so a 1 MiB
  image scrolls like a 4 KiB one. Go-to address; the selected map's bytes are
  highlighted.
- **Map view** and **All maps**: calibration tables painted directly on the
  canvas, heat-coloured, with axis breakpoints and a hover tooltip (cell,
  value, axis values, address). Off-screen cells are culled.
- **Checksum verify** side panel (any library algorithm; the definition's
  algorithm is preselected).
- **Frame timing**: `--bench hex|maps` scrolls a scene by `--px` points per
  frame for `--frames` frames (after a 30-frame warm-up), prints a summary in
  the same terms as the webview's Render check and writes every frame to
  `--csv`.

The map decoding here is a thin local copy of what
`desktop/src-tauri/src/maps.rs` does. The plan is to move that logic into a
shared core crate used by both frontends.

## Build and run

It is not part of the root workspace (own `[workspace]` and lockfile):

```sh
cd native
cargo run --release -- path/to/dump.bin --def bosch-edc15c6
cargo run --release -- path/to/dump.bin --def bosch-edc15c6 \
    --bench hex --frames 600 --px 24 --csv hex.csv [--no-vsync]
```

The ECU definitions are read from `ecudefs/` or `../ecudefs/` (or `--defs`).
On Linux it runs on Wayland or X11 (unset `WAYLAND_DISPLAY` to force X11).

## Measuring

The summary line reports fps, p50 / p95 / max frame interval, frames over
16.7 ms, frames over 25 ms (a real missed vsync at 60 Hz; with vsync on,
jitter puts about half the frames just over 16.7 ms), frames over 50 ms (the
webview's "long" frame) and the CPU time spent building the UI per frame.
