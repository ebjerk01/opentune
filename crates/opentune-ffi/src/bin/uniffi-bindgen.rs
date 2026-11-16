//! The UniFFI binding generator for the OpenTune FFI crate.
//!
//! Run in *library mode* against the compiled cdylib to emit the Kotlin (or
//! Swift / Python / …) bindings, e.g.:
//!
//! ```sh
//! cargo build --release
//! cargo run --bin uniffi-bindgen -- generate \
//!     --library "$CARGO_TARGET_DIR/release/libopentune_ffi.so" \
//!     --language kotlin --config crates/opentune-ffi/uniffi.toml \
//!     --out-dir android/app/src/main/java
//! ```
//!
//! This is the standard UniFFI "bring your own bindgen" entry point so the
//! generator version always matches the runtime (`uniffi`) version.
fn main() {
    uniffi::uniffi_bindgen_main()
}
