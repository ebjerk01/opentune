# Android app plan

The Android app does **not** reimplement any ECU logic. It reuses the exact same
`opentune-core` (driver trait, protocols, ECU definitions, checksum and the
safety orchestrator) as the CLI and desktop app, exposed to Kotlin through
**UniFFI**, with a thin **Kotlin/Compose** shell on top.

> **Status: planned** (roadmap M7). This document is the design, not a shipped
> app.

## Why UniFFI

[UniFFI](https://mozilla.github.io/uniffi-rs/) generates a safe foreign-function
binding (and idiomatic Kotlin) from the Rust side, so the phone app calls into
the same compiled core instead of carrying its own copy of the logic. One core,
one set of safety rules, one ECU-definition library — on desktop and on Android.

## Shape

```
  ┌──────────────────────────────────────────────┐
  │  Kotlin / Jetpack Compose UI                   │
  │   connect · identify · read · backup · flash   │
  └───────────────────────┬──────────────────────┘
                          │  generated Kotlin bindings (UniFFI)
  ┌───────────────────────┴──────────────────────┐
  │  opentune-mobile  (Rust cdylib, UniFFI)        │
  │   wraps opentune-core / -checksum / -flash     │
  └───────────────────────┬──────────────────────┘
                          │  Interface trait
  ┌───────────────────────┴──────────────────────┐
  │  Transport drivers on Android                  │
  │   USB-OTG serial/CAN · BLE (OpenTune device)   │
  └───────────────────────────────────────────────┘
```

## Pieces

1. **`opentune-mobile` crate** (added under `android/` or `crates/`): a UniFFI
   wrapper that re-exports the core operations across the FFI boundary —
   enumerate drivers, open a connection, read ECU id, read/backup a region, and
   run `flash_calibration` with progress callbacks. Built as a `cdylib` for the
   Android ABIs (`arm64-v8a`, `armeabi-v7a`, `x86_64`) with `cargo-ndk`.
2. **UniFFI interface definition** describing the types crossing the boundary
   (`EcuId`, `MemoryRegion`, `Progress`/`Phase`, errors) so the generated Kotlin
   is strongly typed and progress callbacks map to Kotlin callbacks/flows.
3. **Transport drivers for Android**: connectivity differs from desktop, so the
   mobile build supplies its own `Interface`/`CanLink` implementations over
   **USB-OTG** (serial/CAN adapters, the OpenTune device's USB-CDC) and **BLE**
   (the OpenTune device's wireless bridge). The protocol stack above them is the
   core's, unchanged.
4. **Kotlin/Compose shell**: screens for connect, identify, read/backup, a
   read-only map viewer first, then a flash wizard that surfaces the same safety
   checklist the core enforces (voltage gate, verified backup, read-back verify).

## Reuse guarantees

- The safety orchestrator (`opentune-flash`) runs **inside** the core, so the
  Android app cannot skip the voltage gate, backup or verify.
- ECU definitions are the same `ecudefs/*.json`, shipped as app assets and parsed
  by the core, not re-encoded in Kotlin.
- Checksums use the same `opentune-checksum` algorithms as desktop.

## Build outline

```sh
# in the mobile crate
cargo install cargo-ndk uniffi-bindgen
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 build --release
uniffi-bindgen generate <udl-or-library> --language kotlin --out-dir <android-src>
```

The generated `.so` files and Kotlin bindings are bundled into the Gradle
project; the Compose app depends only on the generated Kotlin API.
