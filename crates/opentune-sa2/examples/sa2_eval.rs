//! Batch evaluator for the differential harness in `tools/sa2/`.
//!
//! Reads lines `<script-hex> <seed-hex>` from stdin and prints, per line,
//! the 8-hex-digit key or `ERR <reason>`. Blank lines and `#` comments are
//! echoed as blank lines so output lines stay aligned with input lines.
use std::io::{self, BufRead, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.split('#').next().unwrap_or("").trim().to_string();
        let mut words = line.split_whitespace();
        let (Some(script), Some(seed)) = (words.next(), words.next()) else {
            let _ = writeln!(out);
            continue;
        };
        let result = opentune_sa2::parse_hex(script).and_then(|s| {
            let seed = opentune_sa2::parse_hex(seed)?;
            opentune_sa2::execute(&seed, &s)
        });
        let _ = match result {
            Ok(key) => writeln!(
                out,
                "{}",
                key.iter().map(|b| format!("{b:02X}")).collect::<String>()
            ),
            Err(e) => writeln!(out, "ERR {e}"),
        };
    }
}
