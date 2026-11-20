//! Windows only: embeds the app icon and version info into opentune-native.exe
//! (Explorer, the taskbar and the file properties read them from the exe; the
//! window icon at runtime is set separately in `run`). Every other target
//! returns at once, so Linux and macOS builds do nothing here.
//!
//! The resource is compiled with embed-resource: for the gnu / gnullvm cross
//! build that is `<arch>-w64-mingw32-windres` (llvm-mingw's llvm-windres) from
//! PATH, overridable with `RC_<target>` or `RC`; for msvc, rc.exe / llvm-rc.
//! The icon is the committed packaging/windows/opentune.ico (regenerate with
//! packaging/windows/make-ico.py), so the build needs no image tooling.

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let ico = manifest.join("packaging/windows/opentune.ico");
    println!("cargo:rerun-if-changed={}", ico.display());

    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let rc = out.join("opentune-native.rc");
    fs::write(&rc, resource_script(&ico)).expect("write opentune-native.rc");

    match embed_resource::compile(&rc, embed_resource::NONE) {
        embed_resource::CompilationResult::NotAttempted(why)
            if env::var("PROFILE").as_deref() != Ok("release") =>
        {
            // A debug / check build without a resource compiler still works,
            // just without the exe icon; a release build must not ship so.
            println!("cargo:warning=no Windows resource compiler ({why}); exe built without icon");
        }
        r => r
            .manifest_required()
            .unwrap_or_else(|e| panic!("Windows resource: {e}")),
    }
}

/// The .rc source: the icon (first icon = the one Explorer shows) and a
/// VERSIONINFO block filled from Cargo.toml.
fn resource_script(ico: &std::path::Path) -> String {
    let num = |k: &str| -> u16 { env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(0) };
    let (major, minor, patch) = (
        num("CARGO_PKG_VERSION_MAJOR"),
        num("CARGO_PKG_VERSION_MINOR"),
        num("CARGO_PKG_VERSION_PATCH"),
    );
    let version = rc_str(&env::var("CARGO_PKG_VERSION").unwrap_or_default());
    let license = rc_str(&env::var("CARGO_PKG_LICENSE").unwrap_or_default());
    let bin = rc_str(&format!(
        "{}.exe",
        env::var("CARGO_PKG_NAME").unwrap_or_default()
    ));
    let ico = rc_str(&ico.to_string_lossy().replace('\\', "/"));
    format!(
        r#"#pragma code_page(65001)
1 ICON "{ico}"

1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK 0x3f
FILEFLAGS 0x0
FILEOS 0x40004
FILETYPE 0x1
FILESUBTYPE 0x0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "OpenTune"
      VALUE "FileDescription", "OpenTune"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{bin}"
      VALUE "LegalCopyright", "{license}"
      VALUE "OriginalFilename", "{bin}"
      VALUE "ProductName", "OpenTune"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}

/// Escapes a value for an .rc string literal (`"` doubles, `\` escapes).
fn rc_str(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\"\"")
}
