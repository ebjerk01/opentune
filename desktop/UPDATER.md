# Desktop auto-update — operator & CI note

This is the ground-truth note for OpenTune Desktop's self-update. It covers what
the app now does at runtime, the one CI secret that signing needs, and three
things that are easy to conflate but are **not** the same.

## What the app does (runtime flow)

The Tauri updater **plugin** is registered in `src-tauri/src/main.rs`
(`tauri_plugin_updater`), its endpoint and minisign **public** key live in
`src-tauri/tauri.conf.json`, and the `updater:default` permission is granted in
`src-tauri/capabilities/default.json`. The *invocation* layer is implemented as
Tauri commands and driven from the **About & updates** panel:

| Command | Does |
| --- | --- |
| `update_status` | Reports the running version and whether a long op blocks an update. |
| `update_check` | Queries the endpoint; returns *available* (with version + notes) or *up to date*, or an error (e.g. offline). **No download.** |
| `update_install` | Downloads, **verifies the signature**, installs; emits `update://progress` (`{ downloaded, total }`). |
| `update_relaunch` | Restarts to apply, only after the user confirms. |

User-facing states in the panel: **checking → available / up-to-date / failed →
downloading (progress bar) → installed (restart) / failed**. The user confirms
before any download and again before the restart.

### Signature verification is not hand-rolled

`update_install` does **not** verify signatures itself. The plugin's `download()`
verifies every downloaded bundle against the `pubkey` in `tauri.conf.json`
*before* it is written over the installed app. A tampered bundle, a wrong key, or
a truncated/interrupted download makes that step return an error, which the UI
shows as "Update failed — nothing was installed; your current version is
unchanged." Do not add a second, weaker check that could mask this.

### Update never races a destructive operation

`update_install` claims a single **long-operation guard** (`OpGuard` in
`main.rs`). While a `flash` or a map write-back (`map_write` / `mapdef_write`)
holds that guard, an update is refused, and while an update runs those are
refused. This prevents an in-place binary replacement from landing mid-flash.
The guard frees automatically when the operation returns, errors, or panics
(RAII token), so a failed op can never wedge the app as "busy".

### User files survive an update

An update replaces **only the installed OpenTune application**. It does not touch:

- flash dumps / `.bin` files you open,
- `.otproj` tuning projects (plain JSON side-files you choose where to save),
- `opentune-vault/` safety backups (written next to the edited image),
- the bundled `ecudefs/` / `catalog/` / `guide` resources (shipped inside the new
  app),
- the recent-projects list (browser `localStorage`, same app origin, survives).

The app writes **no** config/registry state of its own, so there is nothing of
the user's for an update to migrate or clobber.

## The one CI secret: `TAURI_SIGNING_PRIVATE_KEY`

To emit **signed** update artifacts, the build needs the minisign **private** key
that matches the `pubkey` in `tauri.conf.json`:

```sh
# one-time, locally, kept OFF the repo:
cargo tauri signer generate -w opentune-updater.key   # prints the public key → tauri.conf.json pubkey

# at build time:
export TAURI_SIGNING_PRIVATE_KEY="$(cat opentune-updater.key)"   # or the key's own text
export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="…"                    # only if the key has a password
cargo tauri build                                                # createUpdaterArtifacts=true emits *.sig + latest.json
```

In CI, provide **`TAURI_SIGNING_PRIVATE_KEY`** (and, if set,
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`) as encrypted repository/organization
secrets, exported into the build job's environment. Rules:

- The private key and its password are **never** committed, printed in logs,
  echoed, or embedded in any artifact. Only the **public** key is in the repo.
- If the key ever leaks, rotate it: generate a new pair, update `pubkey`, and
  ship a release signed with the new key (clients on the old key can still verify
  *that* release only if it is signed with the old key — plan a transition).
- The release workflow (`.github/workflows/release.yml`) passes both secrets to
  the one build step that signs, and **refuses to run a tag build without
  `TAURI_SIGNING_PRIVATE_KEY`** (an unsigned release would strand every
  installed app). See *Publishing a signed release* below.

## Three separate things — do not conflate

1. **Updater signing** (this note): minisign over the *update bundle*, verified by
   the app's updater against `pubkey`. It proves "this update came from whoever
   holds the OpenTune updater key." It is **not** an OS trust anchor.
2. **Installer code-signing / notarization**: platform signing of the *installer*
   itself — an Apple Developer ID cert + notarization on macOS, an Authenticode
   cert on Windows. This is what stops Gatekeeper / SmartScreen warnings on first
   install. It uses **different** keys/certs and **different** secrets, and is not
   configured here. Updater signing does not provide it, and vice-versa.
3. **Reproducible builds**: whether an independent rebuild yields byte-identical
   artifacts. OpenTune Desktop makes **no reproducibility claim** — these builds
   are not reproducible, and a signature over a non-reproducible artifact only
   attests the signer, not an independently verifiable build. Do not describe the
   signed update as "reproducible."

## The endpoint, the key and the release channel

- **Endpoint** (`plugins.updater.endpoints`):
  `https://github.com/ebjerk01/opentune/releases/latest/download/latest.json`.
  GitHub resolves `releases/latest/` to the newest release that is **neither a
  draft nor a pre-release**. Consequences:
  - A release reaches installed apps only once it is **published** (the
    workflow creates a draft; publishing it is the go-live switch).
  - A tag with a pre-release part (`v0.4.0-alpha.1`) becomes a GitHub
    **pre-release** and is **never offered** by the updater. Testers install it
    by hand; it still updates itself to the next full release
    (`0.4.0-alpha.1 < 0.4.0` in semver).
  - **Decision for the alpha period:** ship the alpha series as plain `v0.x.y`
    tags (full releases; the workflow titles every `0.x` release
    "OpenTune v0.x.y (early alpha)"). Use `-alpha.N`/`-rc.N` tags only for builds
    that must *not* auto-update anyone. A separate opt-in beta channel would need
    a second endpoint (e.g. a fixed `updater-beta` release holding its own
    `latest.json`) and is not implemented.
- **Public key** (`plugins.updater.pubkey`): base64 of a two-line minisign public
  key, key id `C273EE936EDA2C76` (Ed25519, `Ed` algorithm, 32-byte key —
  verified 2026-11-18). Do not change it: every installed app trusts exactly this
  key. `scripts/make-latest-json.py --check-config` validates its format, and the
  publish job refuses any `.sig` made by another key id.

## Publishing a signed release

What CI produces for a tag `vX.Y.Z`, in one GitHub release:

| Asset | Purpose |
| --- | --- |
| `OpenTune_X.Y.Z_amd64.AppImage` + `.sig` | Linux install + update bundle (`linux-x86_64`, `linux-x86_64-appimage`) |
| `OpenTune_X.Y.Z_amd64.deb` + `.sig` | Debian/Ubuntu install + update (`linux-x86_64-deb`) |
| `OpenTune_X.Y.Z_x64-setup.exe` + `.sig` | Windows NSIS install + update (`windows-x86_64`, `windows-x86_64-nsis`) |
| `OpenTune_X.Y.Z_x64_en-US.msi` + `.sig` | Windows MSI (`windows-x86_64-msi`; full releases only — WiX rejects `-alpha` versions) |
| `opentune-vX.Y.Z-<target>.tar.gz` / `.zip` | the CLI (Linux, Windows, macOS x86_64 + arm64) |
| `latest.json` | the updater manifest the app polls |
| `SHA256SUMS.txt` | checksums of every asset above |

macOS desktop bundles are **not** built: without an Apple Developer ID the
downloaded `.app` is refused by Gatekeeper on Apple silicon ("damaged"), and
there is no Mac to test on. The `latest.json` generator already understands
`darwin-*` bundles, so adding a signed macOS job later is additive.

### One-time: add the two repository secrets

GitHub → repository **Settings → Secrets and variables → Actions → New
repository secret**:

| Secret | Value |
| --- | --- |
| `TAURI_SIGNING_PRIVATE_KEY` | the private key file's full text (from the password manager) |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | its password (leave the secret out, or empty, only if the key has none) |

Optional repository **variable** (not a secret): `OPENTUNE_RELEASE_AUTO_PUBLISH`
= `true` publishes the release immediately instead of leaving a draft. Leave it
unset until a few releases have gone through the draft review.

### Every release

1. **Dry run (optional, recommended for the first release):** Actions →
   *Release* → *Run workflow* on `main`. Same builds, **unsigned** (`--no-sign`),
   nothing published; the bundles are downloadable as workflow artifacts.
2. **Bump the version** on a branch, merge to `main`:

   ```sh
   scripts/bump-version.sh 0.2.0      # Cargo.toml, desktop Cargo.toml,
                                      # tauri.conf.json, package.json, Cargo.locks
   scripts/bump-version.sh --check    # CI runs this on every push too
   ```

3. **Tag `main` and push the tag** (the tag must equal the version, with a `v`):

   ```sh
   git tag -a v0.2.0 -m "OpenTune 0.2.0" && git push origin v0.2.0
   ```

4. **CI** (`release.yml`): `prepare` checks tag = every version field, the
   endpoint/pubkey, and that `TAURI_SIGNING_PRIVATE_KEY` exists (fails otherwise)
   → `cli` + `desktop` build in parallel; the desktop build signs every bundle →
   `publish` verifies each `.sig` against the app's pubkey (Ed25519, key id must
   be `C273EE936EDA2C76`), writes `latest.json` + `SHA256SUMS.txt` and creates the
   **draft** release with all assets (pre-release if the tag has a `-` part).
5. **Review the draft** (assets, notes; `latest.json` URLs point at
   `releases/download/v0.2.0/...`), then **Publish release** with *Set as the
   latest release* ticked. From this moment installed apps are offered the
   update.
6. **Verify from an older build:** start an installed older OpenTune (e.g. the
   AppImage of the previous release, or the Windows install) → **About &
   updates** → *Check for updates*. Expect *available: 0.2.0* → *Install* →
   progress → *Restart* → the About panel shows 0.2.0. Quick server-side checks:

   ```sh
   curl -sL https://github.com/ebjerk01/opentune/releases/latest/download/latest.json | jq .version
   sha256sum -c SHA256SUMS.txt     # in a folder with the downloaded assets
   ```

   For the **first** signed release there is no older *published* build yet: use
   any build of an older version that carries the same `pubkey` + endpoint, e.g.
   the AppImage from the step-1 dry run made *before* the bump (or a local
   `cargo tauri build` of the previous commit). Updater signing covers the
   download, so the old build itself does not need to be signed.

If a run fails after the draft was created, fix and re-run the failed jobs: the
publish step replaces the draft's assets. It refuses to touch a release that is
already published — cut a new patch version instead.

## What still needs the owner

- Add the secrets `TAURI_SIGNING_PRIVATE_KEY` and
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` (names exactly as written).
- Choose the first version (`scripts/bump-version.sh`), tag, review and publish
  the draft.
- Decide on the channel policy above (plain `v0.x.y` for alphas is the
  recommendation) and whether a beta channel is worth a second endpoint.
- Optional: Windows Authenticode certificate (SmartScreen warning otherwise) and
  an Apple Developer ID if macOS desktop builds are wanted — separate secrets,
  separate work.
- Run the first real end-to-end update (step 6) on the release after the first
  signed one.
