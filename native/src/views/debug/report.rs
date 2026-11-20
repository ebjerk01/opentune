//! Diagnostics report upload (row 39) through the embedding build's
//! [`DiagnosticsSink`] (`NativeConfig::diagnostics`). The public build has
//! none, so nothing is ever sent from it.
//!
//! With a sink, every send needs consent: [`prepare`] assembles the report
//! and REDACTS it ([`opentune_core::redact::redact`]: paths, user / host
//! name, e-mail, IPs, VINs, part numbers, SecurityAccess seeds / keys, long
//! hex) and returns a [`Pending`] that the view shows in full together with
//! the sink's destination. Only [`Pending::send`], called from the user's
//! "Send" click on that preview, reaches the sink, and it consumes the
//! preview: the next report asks again. No telemetry, no automatic mode.

use crate::host::{BuildChannel, DiagnosticsSink, LinkInfo};
use opentune_core::redact::{redact, RedactContext};
use std::sync::Arc;

/// What the redactor knows about this machine (home, user and host name).
pub fn redact_context() -> RedactContext {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let host = env("HOSTNAME").or_else(|| env("COMPUTERNAME")).or_else(|| {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    });
    RedactContext {
        home: env("HOME").or_else(|| env("USERPROFILE")),
        user: env("USER").or_else(|| env("USERNAME")),
        host,
        secrets: Vec::new(),
    }
}

/// The report text before redaction: app, channel, OS, the adapter link
/// (driver and state, not the port) and the session log.
pub fn assemble(channel: &BuildChannel, link: &LinkInfo, note: &str, session: &[String]) -> String {
    let mut s = format!(
        "OpenTune native {} ({} channel)\nOS: {} {}\nAdapter: {} ({})\n",
        env!("CARGO_PKG_VERSION"),
        channel.as_str(),
        std::env::consts::OS,
        std::env::consts::ARCH,
        if link.driver.is_empty() {
            "none"
        } else {
            &link.driver
        },
        if link.connected {
            "connected"
        } else {
            "not connected"
        },
    );
    if !note.trim().is_empty() {
        s.push_str(&format!("\nWhat happened:\n{}\n", note.trim()));
    }
    s.push_str(&format!("\nSession log ({} lines):\n", session.len()));
    for l in session {
        s.push_str(l);
        s.push('\n');
    }
    s
}

/// A redacted report waiting for the user's decision. Only [`prepare`]
/// makes one.
pub struct Pending {
    /// Where it would go (the sink's own description).
    pub destination: String,
    /// Exactly the text that would be sent.
    pub report: String,
    sink: Arc<dyn DiagnosticsSink>,
}

/// Redact `raw` and pair it with `sink`'s destination for the preview.
/// Sends nothing.
pub fn prepare(sink: Arc<dyn DiagnosticsSink>, raw: &str, ctx: &RedactContext) -> Pending {
    Pending {
        destination: sink.destination(),
        report: redact(raw, ctx),
        sink,
    }
}

impl Pending {
    /// The user clicked Send on this preview: submit it (blocking; run it
    /// as a job). Consumes the preview.
    pub fn send(self) -> Result<String, String> {
        self.sink.submit(&self.report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink(Mutex<Vec<String>>);
    impl DiagnosticsSink for Sink {
        fn destination(&self) -> String {
            "https://diag.example.org/upload".into()
        }
        fn submit(&self, report: &str) -> Result<String, String> {
            self.0.lock().unwrap().push(report.to_string());
            Ok("ticket-1".into())
        }
    }

    #[test]
    fn nothing_is_sent_before_consent_and_the_preview_is_what_is_sent() {
        let sink = Arc::new(Sink::default());
        let link = LinkInfo {
            driver: "elm327obd".into(),
            port: "/home/alice/dev/ttyUSB0".into(),
            connected: true,
            ..Default::default()
        };
        let raw = assemble(
            &BuildChannel::new(BuildChannel::ALPHA),
            &link,
            "failed reading /home/alice/dumps/car.bin on alicebox",
            &["12:00:00 [7E0] -> 27 01  <- 67 01 1A 2B 3C 4D  (12 ms)".into()],
        );
        assert!(!raw.contains("ttyUSB0"), "the port is not part of a report");
        let ctx = RedactContext {
            home: Some("/home/alice".into()),
            user: Some("alice".into()),
            host: Some("alicebox".into()),
            secrets: Vec::new(),
        };
        let p = prepare(sink.clone(), &raw, &ctx);
        assert!(sink.0.lock().unwrap().is_empty(), "prepare sends nothing");
        assert_eq!(p.destination, "https://diag.example.org/upload");
        for leak in ["/home/alice", "alicebox", "1A 2B 3C 4D"] {
            assert!(!p.report.contains(leak), "{leak} in {}", p.report);
        }
        let shown = p.report.clone();
        assert_eq!(p.send(), Ok("ticket-1".into()));
        assert_eq!(*sink.0.lock().unwrap(), [shown], "exactly the preview");
        let c = redact_context();
        assert!(c.secrets.is_empty());
    }
}
