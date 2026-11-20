//! The Debug session log: every raw exchange, simulator request, auto-debug
//! step and DoIP discovery of this run, one line each (the desktop's
//! `DebugLog`). In memory only; the user copies or saves it.
//!
//! SecurityAccess (service 0x27) seeds and keys and long hex payloads are
//! masked before a line is stored ([`opentune_core::redact::mask_secrets`],
//! the diagnostics log's masker), so they never reach a copy, a saved file
//! or a report.

use std::sync::Mutex;

/// Lines kept (oldest dropped first).
pub const MAX_LINES: usize = 2000;

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// `HH:MM:SS` (UTC) of now.
fn clock() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

/// Append one line (masked, time-stamped).
pub fn push(line: impl AsRef<str>) {
    let line = format!(
        "{} {}",
        clock(),
        opentune_core::redact::mask_secrets(line.as_ref())
    );
    if let Ok(mut v) = LOG.lock() {
        v.push(line);
        if v.len() > MAX_LINES {
            let extra = v.len() - MAX_LINES;
            v.drain(..extra);
        }
    }
}

/// A copy of the log.
pub fn lines() -> Vec<String> {
    LOG.lock().map(|v| v.clone()).unwrap_or_default()
}

/// The log as text (one line each, newline-terminated).
pub fn text() -> String {
    let mut s = lines().join("\n");
    if !s.is_empty() {
        s.push('\n');
    }
    s
}

pub fn clear() {
    if let Ok(mut v) = LOG.lock() {
        v.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_masked_and_bounded() {
        // The log is process-wide: look only at this test's own lines.
        push("[7E0] -> 27 01  <- 67 01 1A 2B 3C 4D  (12 ms)");
        let l = lines();
        let mine = l.iter().rev().find(|x| x.contains("-> 27 01")).unwrap();
        assert!(mine.contains("<masked: 4 bytes>"), "{mine}");
        assert!(!mine.contains("1A 2B 3C 4D"), "{mine}");
        assert_eq!(mine.as_bytes()[2], b':', "time stamp first: {mine}");
        for i in 0..(MAX_LINES + 5) {
            push(format!("bound {i}"));
        }
        assert!(lines().len() <= MAX_LINES);
        assert!(text().ends_with('\n'));
    }
}
