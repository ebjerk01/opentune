# OpenTune Desktop

A [Tauri](https://tauri.app) app that puts a UI on the shared OpenTune Rust
core. The backend (`src-tauri/`) exposes a handful of commands — `list_drivers`,
`connect`, `identify`, `read_region` — that call straight into `opentune-core`,
`opentune-checksum` and `opentune-flash`; the frontend is plain HTML/JS, no
build step.

> This crate is **not** part of the root Cargo workspace. It builds on its own so
> the workspace stays lean and the desktop-only dependencies (Tauri) don't leak
> into the core build.

## Layout

```
desktop/
├── index.html          # UI markup
├── src/main.js         # UI logic — calls Tauri commands
├── package.json        # Tauri CLI wrapper scripts
└── src-tauri/
    ├── Cargo.toml      # depends on the core + driver crates by relative path
    ├── tauri.conf.json # window + bundle + updater config
    ├── build.rs
    └── src/main.rs     # #[tauri::command]s that call the core
```

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

The frontend is served directly from `index.html` (`build.frontendDist` is
`"../"`), so there is nothing to bundle on the web side.

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

Until a real keypair is generated and `pubkey` is filled in, the updater is
configured but will not verify — treat the placeholder as "not yet signed".

## Licence

GPL-3.0-or-later, like the rest of OpenTune.
