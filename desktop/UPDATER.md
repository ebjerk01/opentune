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
- Wiring this into `.github/` workflows is deliberately **out of scope for this
  note** — see `.github/` (owned separately). This note only states the
  requirement.

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

## Status

The updater is wired end to end (plugin + endpoint + real `pubkey` + capability +
invocation UI + guard). What remains before releases work:

- Hold the matching **private** key (do not lose it) and expose
  `TAURI_SIGNING_PRIVATE_KEY` to the build/CI.
- Confirm `endpoints` in `tauri.conf.json` points at the real release URL.
- Publish a signed release (installers + update bundles + `.sig` + `latest.json`)
  — this is the only way to exercise a real end-to-end update, and it was **not**
  done as part of this offline change.
