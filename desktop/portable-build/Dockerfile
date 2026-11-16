# Reproducible, portable OpenTune desktop build environment.
#
# WHY THIS EXISTS
# An AppImage bundles the app's own libraries but NOT glibc, and glibc is
# forward-incompatible: a binary compiled against glibc 2.39 will NOT start on a
# machine with glibc 2.36, failing with errors like
#   version `GLIBC_2.39' not found (required by ...)
# So the build host's glibc is the MINIMUM version every user needs. Building on
# a bleeding-edge distro (e.g. Ubuntu 25/26) produces an AppImage that runs
# almost nowhere.
#
# Building on debian:bookworm pins the baseline to glibc 2.36 (Debian 12), which
# also ships webkit2gtk-4.1. The resulting AppImage runs on Debian 12+,
# Ubuntu 24.04+, Fedora 37+ and most current distros. Need older systems
# (Debian 11 / Ubuntu 22.04, glibc 2.31/2.35)? Change the base image to
# debian:bullseye or ubuntu:22.04 and use libwebkit2gtk-4.0-dev instead.
FROM debian:bookworm

ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential curl ca-certificates pkg-config file patchelf \
      libwebkit2gtk-4.1-dev libgtk-3-dev librsvg2-dev libssl-dev \
      libudev-dev \
      libayatana-appindicator3-dev desktop-file-utils fuse libfuse2 \
    && rm -rf /var/lib/apt/lists/*

# Rust (stable) + the Tauri v2 CLI.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable
ENV PATH=/root/.cargo/bin:$PATH
RUN cargo install tauri-cli --version '^2.0' --locked

# linuxdeploy (run by the tauri bundler to assemble the AppImage) needs FUSE; in
# a container we extract instead of mounting.
ENV APPIMAGE_EXTRACT_AND_RUN=1
WORKDIR /src
