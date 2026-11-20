//! "Check for updates" (row 34, owner decision (a)): open the OpenTune
//! releases page in the browser. The app never downloads or installs an
//! update itself; the user reads the release and downloads it from there.
//!
//! The URL goes to the OS opener the Vault view uses for "Reveal"
//! (`xdg-open` / `open` / `explorer`), which hands an `https://` address to
//! the default browser. Only fixed `https://` URLs are opened.

use std::path::Path;

/// Where releases are published.
pub const RELEASES_URL: &str = "https://github.com/ebjerk01/opentune/releases";

/// Is `url` something this module opens (plain `https://`, no spaces or
/// quotes that an opener could read as arguments)?
fn openable(url: &str) -> bool {
    url.starts_with("https://")
        && url.len() > "https://".len()
        && !url
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '`'))
}

/// Open `url` in the default browser (non-blocking).
pub fn open_url(url: &str) -> Result<(), String> {
    if !openable(url) {
        return Err(format!(
            "refusing to open '{url}': not a plain https:// address"
        ));
    }
    crate::views::vault::reveal(Path::new(url))
        .map_err(|e| format!("could not start the browser: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_https_urls_open() {
        assert!(openable(RELEASES_URL));
        for bad in [
            "",
            "https://",
            "http://example.org",
            "file:///etc/passwd",
            "https://x.org/a b",
            "https://x.org/\"a",
            "--help",
        ] {
            assert!(!openable(bad), "{bad}");
            assert!(open_url(bad).is_err(), "{bad}");
        }
    }
}
