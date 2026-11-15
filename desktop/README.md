# OpenTune Desktop

The open-source, **calibration-only** OpenTune app — a [Tauri 2](https://tauri.app)
desktop UI over the shared OpenTune Rust core. It runs the whole workflow
**offline** on a flash-image file through the mock/file driver: open a dump,
identify it, verify/fix checksums, diff for maps, decode a defined map into a
heat grid, run a safety-gated (mock) flash, and browse a backup vault.

It is deliberately calibration-only — there is **no** immobiliser / EEPROM /
standalone / bench tooling here (that lives in a separate private app).

> This crate is **not** part of the root Cargo workspace. It builds on its own so
> the workspace stays lean and the desktop-only dependencies (Tauri) don't leak
> into the core build.

## Panels

`Image` · `Identify` · `Checksum` · `Maps` (findmaps diff + read-only map
viewer) · `Flash` · `Vault` · `Drivers` · `Guide` (bundled offline markdown).

## Backend commands

`list_drivers`, `data_paths`, `image_info`, `hex_preview`, `identify`,
`match_identity`, `checksum_verify`, `checksum_fix` (with a Save-As fallback),
`findmaps`, `list_ecudefs`, `list_maps`, `decode_map`, `flash`, `vault_list`,
`guide_list`, `guide_read` — each calls straight into `opentune-core`,
`opentune-checksum` and `opentune-flash`.

## Layout

```
desktop/
├── ui/                     # frontend (build.frontendDist = "../ui")
│   ├── index.html
│   └── src/{app.js,styles.css}
├── package.json            # Tauri CLI wrapper scripts
└── src-tauri/
    ├── Cargo.toml          # core + driver crates by relative path
    ├── tauri.conf.json     # window + bundle (ecudefs/catalog/guide resources) + updater
    ├── capabilities/default.json   # Tauri 2 core + dialog permissions
    ├── guide/*.md          # bundled in-app guide sections
    ├── icons/              # app icons
    ├── build.rs
    └── src/main.rs         # #[tauri::command]s that call the core
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
