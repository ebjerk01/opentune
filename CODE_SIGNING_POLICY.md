# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io/), certificate by
[SignPath Foundation](https://signpath.org/).

*(Status: the SignPath Foundation application is pending. Until it is approved, Windows
installers are published unsigned and Windows SmartScreen will ask you to confirm
"Run anyway". The desktop app's own update packages are always signed, see below.)*

## What gets signed

Only artifacts built from this repository's source code by the public
[release workflow](.github/workflows/release.yml) on GitHub Actions:

- the Windows desktop installers (NSIS `.exe`, `.msi`) and the desktop executable they contain;
- the Windows command-line tool `opentune.exe`.

No third-party or upstream binaries are signed under this project's certificate, and
nothing built outside the release workflow is submitted for signing. Every signing
request is approved manually by an approver (below) for each release.

Separately, every desktop update package carries a
[Tauri updater signature](desktop/UPDATER.md) (minisign, key id `C273EE936EDA2C76`); the
app refuses updates that do not verify against the public key built into it.

## Team roles

| Role | Members |
|---|---|
| Authors (commit without further review) | [@ebjerk01](https://github.com/ebjerk01) |
| Reviewers (review changes from non-committers) | [@ebjerk01](https://github.com/ebjerk01) |
| Approvers (approve each signing request) | [@ebjerk01](https://github.com/ebjerk01) |

All team members use multi-factor authentication for GitHub and SignPath.
Contributions from others arrive as pull requests and are reviewed before merging.

## Privacy

This program will not transfer any information to other networked systems unless
specifically requested by the user or the person installing or operating it.

The only network access the desktop app makes on its own is the update check: it
downloads `latest.json` from this repository's GitHub Releases (GitHub's
[privacy statement](https://docs.github.com/en/site-policy/privacy-policies/github-general-privacy-statement)
applies to that request). No usage data, vehicle data, ECU images or identifiers are
sent anywhere. Everything else (vehicle adapters, files) stays on your machine.

## Scope

OpenTune is a diagnostics and calibration suite. It ships no manufacturer
security-access (seed/key) algorithms, and immobiliser removal, odometer
manipulation and emissions-control defeat are explicitly out of scope (see the
README). Writes to a vehicle go through a safety policy: verified backup,
voltage check, explicit confirmation and read-back verification.
