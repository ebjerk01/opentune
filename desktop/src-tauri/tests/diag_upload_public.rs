//! The public app ships no diagnostics endpoint and no token.
//!
//! * The diagnostics upload sources (the Rust module outside its test file,
//!   the redactor outside its tests, the UI script) contain no `http://` or
//!   `https://` literal at all, so no endpoint can be compiled in by
//!   accident. Tests use reserved names (`.invalid`) or loopback only.
//! * The public binary's configuration has `diagnostics: None`.

use std::fs;
use std::path::PathBuf;

fn manifest() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// `src` without its `#[cfg(test)] mod tests { … }` block (at the end).
fn without_tests(src: &str) -> &str {
    match src.find("#[cfg(test)]\nmod tests {") {
        Some(i) => &src[..i],
        None => src,
    }
}

fn url_literals(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| {
            let l = l.to_ascii_lowercase();
            l.contains("http://") || l.contains("https://")
        })
        .map(|(i, l)| (i + 1, l.trim().to_string()))
        .collect()
}

fn upload_sources() -> Vec<PathBuf> {
    let dir = manifest().join("src").join("diagupload");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("src/diagupload")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .filter(|p| p.file_name().is_some_and(|n| n != "tests.rs"))
        .collect();
    files.sort();
    assert!(files.len() >= 4, "{files:?}");
    files.push(manifest().join("../ui/src/diagup.js"));
    files.push(manifest().join("../../crates/opentune-core/src/redact.rs"));
    files
}

#[test]
fn upload_sources_hold_no_endpoint_literal() {
    let mut bad = Vec::new();
    for f in upload_sources() {
        let src = fs::read_to_string(&f).unwrap_or_else(|e| panic!("{}: {e}", f.display()));
        for (line, text) in url_literals(without_tests(&src)) {
            bad.push(format!("{}:{line}: {text}", f.display()));
        }
    }
    assert!(
        bad.is_empty(),
        "the diagnostics upload must not contain an endpoint (URL literal) outside tests:\n{}",
        bad.join("\n")
    );
}

#[test]
fn the_check_itself_finds_a_literal() {
    let sample = format!("let e = \"{}://diag.example.invalid\";\n", "https");
    assert_eq!(url_literals(&sample).len(), 1);
    let with_tests = format!("fn a() {{}}\n#[cfg(test)]\nmod tests {{\n{sample}}}\n");
    assert!(url_literals(without_tests(&with_tests)).is_empty());
}

#[test]
fn the_public_app_configures_no_upload() {
    assert!(opentune_desktop::DesktopConfig::default()
        .diagnostics
        .is_none());
    // The public binary runs the default configuration and sets nothing else.
    let main = fs::read_to_string(manifest().join("src/main.rs")).unwrap();
    for needle in ["DiagUpload", "diagnostics", "OPENTUNE_DIAG"] {
        assert!(!main.contains(needle), "src/main.rs mentions {needle}");
    }
    let lib = fs::read_to_string(manifest().join("src/lib.rs")).unwrap();
    assert!(lib.contains("builder_with(tauri::Builder::default(), DesktopConfig::default())"));
    assert!(lib.contains("diagnostics: None,"));
}
