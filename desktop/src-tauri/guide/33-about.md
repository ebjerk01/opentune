# About & updates

Shows the installed version and checks for a newer signed release.

## What you need

- An internet connection to check or download. Everything else in the app works offline.

## Step by step

1. Press **Check for updates**.
2. If a newer version exists, its version and release notes are shown. Press **Download & install** and confirm.
3. A progress bar shows the download. When done, press **Restart to apply**.

## Reading the results

- **You are up to date** — no newer release.
- **Update check failed** — usually no internet. Try again later.
- **Blocked — "…" is running** — a flash or map write-back is in progress. Updates are not allowed to start until it finishes.

## Diagnostics

Only when this build is set up to upload diagnostics (a plugin build, or the
`OPENTUNE_DIAG_ENDPOINT` environment variable) a **Diagnostics** card is
shown here: where reports go, **Allow sending diagnostics** (off by default),
**Send diagnostics…** with a preview of the exact redacted text, the switch
for automatic sending in a build that has it, and the last uploads. The public
app has no upload target and sends nothing. See the Debug guide and
docs/DIAGNOSTICS.md.

## Safety

- Nothing is downloaded until you ask, and nothing is installed unless its signature is valid.
- An update cannot start while a flash or map write-back is running.
- An update replaces only the application. Your dumps, `.otproj` projects and `opentune-vault` backups are your own files and are not touched.
- Do not update while a live diagnostic operation on a car is running. Finish or stop it first.
