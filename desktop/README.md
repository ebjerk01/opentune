# OpenTune Desktop

The open-source OpenTune app — a [Tauri 2](https://tauri.app) desktop UI over
the shared OpenTune Rust core. The calibration workflow runs **offline** on a
flash-image file through the mock/file driver: open a dump, identify it,
verify/fix checksums, diff for maps, view/edit a defined map as a heat grid, run
a safety-gated (mock) flash, and browse a backup vault.

It also talks to a **live adapter**: a sidebar adapter bar picks the driver /
port and probes what the adapter can do, and the diagnostics panels (coding,
auto-scan + DTCs, adaptation, service functions, live logging, OBD dashboard)
run over its diagnostic transport, with each action gated on the adapter's
reported capabilities. These live paths are **not yet validated on a vehicle**.

There is deliberately **no** immobiliser / EEPROM / standalone / bench tooling
here — it is out of scope for this app.

> This crate is **not** part of the root Cargo workspace. It builds on its own so
> the workspace stays lean and the desktop-only dependencies (Tauri) don't leak
> into the core build.

## Panels

`Image` · `Analyze bin` · `Identify` · `Checksum` · `Maps` (findmaps diff +
map viewer/editor) · `Compare` · `Flash` · `Vault` · `Drivers` · `Project` ·
`Utilities` · `Guide` (bundled offline markdown) · `About & updates`.

Diagnostics (live, capability-gated): `Coding` · `Auto-scan` (modules + DTCs) ·
`Adaptation` · `Service fn` · `Live log` · `OBD live` (dashboard) ·
`Auto-debug` (a guided adapter self-test; its raw-request steps need an alpha
build).

**Alpha builds** (`--features alpha`) add only the `Debug` panel: raw UDS/OBD
requests to the adapter, the offline UDS simulator, and a session log. Every
other panel is in the default build.

## Backend commands

Grouped by area (see the `generate_handler!` list in `src-tauri/src/main.rs`
for the full set); each calls straight into the OpenTune crates.

- **Image & calibration** — `image_info`, `hex_preview`, `analyze_image`,
  `identify`, `match_identity`, `checksum_verify`, `checksum_fix` (with a
  Save-As fallback), `findmaps`, `list_ecudefs`, `list_maps`, `decode_map`,
  `decode_mapdef`, `map_write`, `mapdef_write`, `compare_maps`,
  `import_definitions`, `promote_region`, `project_save` / `project_open`.
- **Flash & vault** — `flash`, `vault_list`.
- **Adapter** — `list_drivers`, `probe_adapter`, `build_channel`.
- **Diagnostics engines** (`feat_*.rs`) — `coding_read` / `_set_preview` /
  `_write`, `autoscan_run` / `_clear`, `adaptation_list` / `_read` /
  `_actuator` / `_routine`, `service_plan` / `_run`, `log_params` /
  `log_poll_once`, `obd_live`.
- **Utilities** — `vin_decode`, `part_decode`, `dtc_lookup`, `seedkey_algos`,
  `seedkey_compute`, `data_paths`, `guide_list`, `guide_read`.
- **Debug** — `debug_raw_request` and `debug_simulate` (refuse to run unless
  built with the `alpha` feature), plus `debug_log_dump` / `debug_log_clear`
  for the session log.

Self-update is driven by `update_status`, `update_check`, `update_install`
(emits an `update://progress` event) and `update_relaunch`. A shared
long-operation guard (`OpGuard`) makes `flash` / `map_write` / `mapdef_write` and
an update mutually exclusive, so an in-place binary replacement can never race a
destructive write. See [`UPDATER.md`](UPDATER.md).

## Layout

```
desktop/
├── ui/                     # frontend (build.frontendDist = "../ui")
│   ├── index.html
│   └── src/{app.js,ot-caps.js,feat_*.js,styles.css}
├── package.json            # Tauri CLI wrapper scripts
└── src-tauri/
    ├── Cargo.toml          # core + driver crates by relative path
    ├── tauri.conf.json     # window + bundle (ecudefs/catalog/guide resources) + updater
    ├── capabilities/default.json   # Tauri 2 core + dialog permissions
    ├── guide/*.md          # bundled in-app guide sections
    ├── icons/              # app icons
    ├── build.rs
    └── src/{main.rs,feat_*.rs}   # #[tauri::command]s that call the core
```

The ECU definitions (`ecudefs/`), vehicle catalog (`catalog/`) and the guide
markdown are bundled as Tauri resources and resolved at startup (a dev build
falls back to the repo-root `ecudefs/` and `catalog/`).

## Prerequisites

- A stable Rust toolchain (same as the workspace).
- The Tauri system dependencies for your OS — see
  <https://tauri.app/start/prerequisites/>.
- Node is only needed for the `@tauri-apps/cli` wrapper. You can also install the
  CLI with Cargo instead:

  ```sh
  cargo install tauri-cli --version "^2.0.0"
  ```

## Build and run

From the `desktop/` directory:

```sh
# with the npm-provided CLI
npm install
npm run dev        # development window with the UI
npm run build      # production bundle + updater artifacts

# …or with the cargo-installed CLI
cargo tauri dev
cargo tauri build
```

The frontend is served directly from `ui/index.html` (`build.frontendDist` is
`"../ui"`), so there is nothing to bundle on the web side.

### Startup

Nothing heavy runs before the window paints. The ECU definitions and the
catalog are parsed once on a background thread and cached. Startup-path
commands are `async`, so they run off the GTK thread. Panels load their data
the first time they are shown (`window.onPanelFirstShow`). On Linux the app
sets `WEBKIT_DISABLE_DMABUF_RENDERER=1` unless that variable is already set.
To opt out, set `OPENTUNE_KEEP_WEBKIT_ENV=1`. If the app starts slowly, run it
with `OPENTUNE_TRACE_STARTUP=1` to get a timing breakdown. See
[docs/TROUBLESHOOTING-STARTUP.md](../docs/TROUBLESHOOTING-STARTUP.md).

## Auto-update (signed GitHub Releases)

The app is configured to update itself from the project's **GitHub Releases**.
In `src-tauri/tauri.conf.json`:

```json
"plugins": {
  "updater": {
    "endpoints": [
      "https://github.com/ebjerk01/opentune/releases/latest/download/latest.json"
    ],
    "pubkey": "REPLACE_WITH_TAURI_UPDATER_PUBLIC_KEY"
  }
}
```

- **Endpoint** — Tauri fetches `latest.json` attached to the latest GitHub
  release. That manifest lists the current version and the per-platform signed
  bundle URLs. `createUpdaterArtifacts: true` makes `tauri build` emit the
  update bundles and their `.sig` files to attach to the release.
- **Signing key** — the updater verifies every downloaded bundle against the
  public key in `pubkey`, so updates must be signed with the matching **private**
  key. Generate a keypair once and keep the private key secret (never commit it):

  ```sh
  cargo tauri signer generate -w opentune-updater.key
  ```

  Put the printed **public** key in `pubkey` above. At build time, provide the
  private key so bundles are signed:

  ```sh
  export TAURI_SIGNING_PRIVATE_KEY="$(cat opentune-updater.key)"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="…"   # if you set one
  cargo tauri build
  ```

  In CI, store the private key and its password as encrypted secrets.

### Release checklist

1. Bump the version in `tauri.conf.json` (and `Cargo.toml`).
2. `cargo tauri build` with the signing key set.
3. Create a GitHub release and attach the installers, the update bundles, their
   `.sig` files, and `latest.json`.
4. Running apps pick up the new version from the endpoint on next check.

A real `pubkey` is already present in `tauri.conf.json`, and the check → confirm
→ download+verify → install → relaunch flow is wired up in the **About & updates**
panel. What is still needed to ship an update is the matching **private** key in
the build/CI (`TAURI_SIGNING_PRIVATE_KEY`) and a published, signed release — see
[`UPDATER.md`](UPDATER.md) for the CI-secret handling and for why updater signing,
installer code-signing/notarization and build reproducibility are three separate
concerns (OpenTune makes **no** reproducibility claim).

## Licence

GPL-3.0-or-later, like the rest of OpenTune.
