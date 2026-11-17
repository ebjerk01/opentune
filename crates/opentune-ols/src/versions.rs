//! Per-version labels recovered from the project's free-text revision tag.
//!
//! WinOLS keeps every saved ROM copy in one file and, by convention, records a
//! label for each inside the parentheses of the `RevisionTag` string (e.g.
//! `"2 (Original, Stage 1, Stage 2)"`). The label is a convenience, not load-
//! bearing: a version with no confidently-split label is still fully usable.

/// One saved ROM version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionInfo {
    /// Zero-based index into the container's version list.
    pub index: u32,
    /// ROM size in bytes (identical for every version in a project).
    pub size: u32,
    /// Human-readable label (`"Original"`, `"Stage 2 (DPFoff)"`, ...) when the
    /// revision tag could be split into exactly the right number of entries;
    /// `None` otherwise (never guessed).
    pub label: Option<String>,
}

/// Split a revision-tag string into one label per version, or `None` when it
/// does not match a known convention or does not split into exactly
/// `expected` entries. Two separators are seen in the wild: `",\t"` (comma +
/// tab) and a plain `","`. An entry of the form `"Name:\tValue"` is rendered
/// `"Name (Value)"`.
pub fn split_labels(revision_tag: &str, expected: u32) -> Option<Vec<String>> {
    if expected == 0 {
        return None;
    }
    let open = revision_tag.find('(')?;
    let close = revision_tag.rfind(')')?;
    let inner = revision_tag.get(open + 1..close)?;

    let render = |entry: &str| -> String {
        match entry.split_once(":\t") {
            Some((name, value)) => {
                let (name, value) = (name.trim(), value.trim());
                if value.is_empty() {
                    name.to_string()
                } else {
                    format!("{name} ({value})")
                }
            }
            None => entry.trim().to_string(),
        }
    };

    for sep in [",\t", ","] {
        let parts: Vec<&str> = inner.split(sep).collect();
        if parts.len() as u32 == expected {
            return Some(parts.into_iter().map(render).collect());
        }
    }
    None
}
