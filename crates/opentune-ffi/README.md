# opentune-ffi

A thin [UniFFI](https://mozilla.github.io/uniffi-rs/) binding that re-exports
OpenTune's **offline** calibration workflow to Kotlin (Android) and any other
UniFFI target. It owns no new logic — every function mirrors an operation
already in the CLI (`crates/opentune-cli`) and the desktop Tauri backend
(`desktop/src-tauri`), driven by the offline mock/file driver so the whole
pipeline runs on a local flash-image `.bin` with no hardware.

## API (proc-macro bindings)

| Function | Purpose |
|----------|---------|
| `open_image(path) -> ImageInfo` | Validate + size a flash image. |
| `identify(path, ecudefs_dir) -> Identity` | Read identity (mock driver) and match a definition. |
| `checksum_verify(path, algo) -> Vec<Block>` | Per-block verify; `algo` is any `opentune_checksum::get` id (`edc16`/`edc17`/`pcr21`/`crc32-tail`/`edc15c6`/`me7`/`dq250`/`dq381`/`tcu-ccitt`/`zf8hp`/`haldex`). |
| `checksum_fix(path, algo, out_path?) -> Vec<Fix>` | Correct checksums in place (or Save-As). |
| `find_maps(stock, tuned) -> Vec<Region>` | Candidate map regions from a stock/tuned diff. |
| `list_ecudefs(ecudefs_dir) -> Vec<DefSummary>` | List definitions (for a picker). |
| `list_maps(ecudef_path) -> Vec<MapSummary>` | List a definition's maps (for a picker). |
| `decode_map(path, ecudef, map) -> Grid` | Decode a map to a physical-value heat grid + axes (read-only). |
| `flash(path, addr, data, min_mv) -> FlashReport` | Safety-gated mock flash (voltage → backup → write → verify). |

`identify` takes an ecudefs **directory**; `list_maps` / `decode_map` take a
single ecudef **JSON file** path (as returned in `DefSummary.path`).

## Verify + generate bindings

```sh
export CARGO_TARGET_DIR=/some/target
cargo test            # 9 tests exercise the real offline pipeline
cargo clippy -- -D warnings
cargo fmt --check
cargo build --release # builds libopentune_ffi.so (cdylib)

# Kotlin bindings (library mode) -> org/opentune/ffi/opentune.kt:
cargo run --bin uniffi-bindgen -- generate \
    --library "$CARGO_TARGET_DIR/release/libopentune_ffi.so" \
    --language kotlin --config uniffi.toml \
    --out-dir ../../android/app/src/main/java
```

