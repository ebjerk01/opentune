# Portable Linux builds (glibc baseline)

A Linux AppImage/binary bundles its own libraries but **not glibc**, and glibc is
*forward-incompatible*: something compiled against glibc 2.39 will not start on a
machine that only has glibc 2.36, failing with

```
version `GLIBC_2.39' not found (required by opentune-...-desktop)
```

So **the build host's glibc is the minimum version every user needs.** Building
the desktop app on a bleeding-edge distro (Ubuntu 25/26, glibc 2.41/2.43)
produces an AppImage that runs almost nowhere.

## The fix

Build against an **old** glibc. This directory pins the baseline to
**glibc 2.36 (Debian 12 "bookworm")**, which also ships webkit2gtk-4.1. The
resulting AppImage runs on Debian 12+, Ubuntu 24.04+, Fedora 37+ and most
current distributions.

```sh
# from the repo root, on any machine with Docker:
desktop/portable-build/build.sh
# → bundles land in ./dist/  (*.AppImage and *.deb)
```

Verify the baseline of a built AppImage:

```sh
cd /tmp && /path/to/OpenTune.AppImage --appimage-extract >/dev/null
objdump -T squashfs-root/usr/bin/opentune-desktop | grep -oE 'GLIBC_[0-9.]+' | sort -V | tail -1
# should print 2.36 or lower
```

## Want to support even older systems?

Change the base image in [`Dockerfile`](./Dockerfile):

| Base image         | glibc | WebKitGTK dev pkg          | Runs on            |
|--------------------|-------|----------------------------|--------------------|
| `debian:bookworm`  | 2.36  | `libwebkit2gtk-4.1-dev`    | Debian 12+ (default) |
| `ubuntu:22.04`     | 2.35  | `libwebkit2gtk-4.0-dev`*   | Ubuntu 22.04+      |
| `debian:bullseye`  | 2.31  | `libwebkit2gtk-4.0-dev`*   | Debian 11+, Ubuntu 20.04+ |

\* On a 4.0 base, Tauri must be built with its `webkit2gtk-4.0`/older feature
path; `4.1` is only on bookworm and newer.

## CI vs. this directory

Two ways to get a portable build, both intentional:

- **CI** (`.github/workflows/release.yml`) pins the Linux desktop and CLI jobs to
  the **`ubuntu-22.04`** runner (**glibc 2.35**) rather than `ubuntu-latest`, so
  published releases run on Debian 12+/Ubuntu 22.04+ and newer. That's the
  zero-infrastructure fix for automated releases.
- **This directory** is the **local, reproducible** path: a `debian:bookworm`
  (glibc 2.36) Docker build you can run on any machine with Docker, handy for a
  one-off build or when you don't want to cut a tag. Its baseline (2.36) is a
  hair higher than CI's (2.35) but still covers Debian 12+ and all current
  distros.

The important rule either way: **never** build the shipped Linux artifacts on a
bleeding-edge distro.
