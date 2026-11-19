//! ELM327 reply parsing shared by the ELM-based drivers.
//!
//! An ELM327 answers a hex request with text terminated by the `>` prompt. With
//! headers off (`ATH0`) and CAN auto-formatting on (`ATCAF1`) one prompt block
//! can hold **several** responses, one per line, for example:
//!
//! ```text
//! 7F2E78          <- responsePending (NRC 0x78) from the ECU
//! 6EF1A0          <- the real positive response, same prompt block
//! >
//! ```
//!
//! or an ISO-TP multi-frame reply, printed as a 3-digit total-length line
//! followed by `N:` frame lines:
//!
//! ```text
//! 014
//! 0:62F190575657
//! 1:5A5A5A314B5A41
//! 2:57303030303031
//! >
//! ```
//!
//! [`parse_block`] is the pure parser (one prompt block → per-response bytes or a
//! structured error) and [`read_reply`] is the request-side read loop built on
//! it, which handles responsePending without waiting for a prompt that a genuine
//! ELM never prints between the pending frame and the real answer.
//!
//! A responsePending only counts when it echoes the service of the request
//! that was sent (the request hex both functions take), the same check as
//! [`diag::is_response_pending`](crate::protocol::diag::is_response_pending):
//! a `7F xx 78` for another service is handed back as the reply, and the
//! client reports it as mis-correlated.
//!
//! Adapter robustness (seen on a real car with a v1.5 clone): every adapter
//! error line (`NO DATA`, `STOPPED`, `BUFFER FULL`, `CAN ERROR`, `?`, ...) is an
//! error, never data; `SEARCHING...` and the K-line init banner are skipped;
//! a stray `>` in the middle of a line is dropped when more text follows; and a
//! power cycle mid-session (the `ELM327 v1.5` banner, possibly after a burst of
//! non-printable noise, or the request echoed back) is
//! [`Error::AdapterReset`]: the caller re-opens the interface.

use crate::protocol::diag::is_response_pending;
use crate::{Error, Result};
use std::time::{Duration, Instant};

pub mod compact;
pub mod monitor;

/// Largest request a CAN ELM327 can send. With `ATCAF1` the adapter wraps the
/// request in a single ISO-TP frame itself and has no way to transmit an
/// outgoing multi-frame (first frame + consecutive frames), so 7 bytes is the
/// hard ceiling.
pub const MAX_CAN_REQUEST: usize = 7;

/// Enhanced response timeout after a responsePending (UDS P2*server ≈ 5000 ms).
pub const P2_STAR_MS: u64 = 5000;

/// Upper bound on how many responsePending frames one request may receive
/// before it is treated as a timeout.
pub const MAX_PENDING: usize = 64;

/// What one block of ELM text contains, after errors have been ruled out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ElmBlock {
    /// The reply: the first complete response that is **not** a
    /// responsePending for the request, if any. When several ECUs answer (a
    /// functional request), a positive response for the request's service is
    /// preferred over another ECU's negative one. Responses are never
    /// concatenated.
    pub response: Option<Vec<u8>>,
    /// Number of responsePending (`7F <sid> 78`, `sid` = the request's
    /// service) responses seen.
    pub pending: usize,
    /// A multi-frame reply that has started but not finished:
    /// `(expected_len, bytes_so_far)`. Only reported when no complete
    /// non-pending response precedes it.
    pub incomplete: Option<(usize, usize)>,
}

/// Reject a request a CAN ELM327 cannot transmit (see [`MAX_CAN_REQUEST`]),
/// before anything is written to the adapter.
pub fn check_can_request_len(req: &[u8]) -> Result<()> {
    if req.is_empty() {
        return Err(Error::Protocol("empty diagnostic request".into()));
    }
    if req.len() > MAX_CAN_REQUEST {
        return Err(Error::Unsupported(format!(
            "ELM327 cannot send a {}-byte request: with CAN auto-formatting it only \
             transmits single-frame ISO-TP (max {MAX_CAN_REQUEST} bytes); use a raw-CAN \
             interface (slcan/gs_usb/SocketCAN/J2534) for multi-frame requests",
            req.len()
        )));
    }
    Ok(())
}

/// Hex text of a request, as written to the adapter (`22F190`).
pub fn request_hex(req: &[u8]) -> String {
    req.iter().map(|b| format!("{b:02X}")).collect()
}

/// A reset banner: the adapter rebooted (power cycle, brown-out, `ATZ`) and is
/// back to defaults (echo on, header 7DF, protocol auto), so the caller has to
/// re-initialise.
fn reset_error(line: &str, raw: &str) -> Option<Error> {
    if line.contains("ELM327") || line.contains("LV RESET") {
        return Some(Error::AdapterReset(format!(
            "ELM327 reset mid-session ({raw}); re-open the interface"
        )));
    }
    None
}

/// Classify one trimmed, upper-cased ELM line as an adapter error, if it is one.
fn line_error(line: &str, raw: &str, sent_hex: &str) -> Option<Error> {
    if let Some(e) = reset_error(line, raw) {
        return Some(e);
    }
    let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
    if !sent_hex.is_empty() && compact == sent_hex.to_ascii_uppercase() {
        // Our own request echoed back: echo is on again, i.e. the adapter lost
        // its init (a reset whose banner we missed).
        return Some(Error::AdapterReset(format!(
            "ELM327 echoed the request {sent_hex}: adapter settings were lost \
             (reset?); re-open the interface"
        )));
    }
    if line == "?" {
        return Some(Error::Protocol(
            "ELM327 did not understand the request (`?`)".into(),
        ));
    }
    if let Some(digits) = line.strip_prefix("ERR") {
        if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            return Some(Error::Protocol(format!("ELM internal error {raw}")));
        }
    }
    const ERRORS: &[&str] = &[
        "NO DATA",
        "UNABLE TO CONNECT",
        "BUS INIT",
        "BUS ERROR",
        "BUS BUSY",
        "CAN ERROR",
        "DATA ERROR",
        "BUFFER FULL",
        "FB ERROR",
        "STOPPED",
        "RX ERROR",
        "ACT ALERT",
        "LP ALERT",
    ];
    for e in ERRORS {
        if line.contains(e) {
            // `BUS INIT: ...OK` is the K-line init banner, not an error.
            if *e == "BUS INIT" && !line.contains("ERROR") {
                continue;
            }
            return Some(Error::Protocol(format!("ELM: {raw}")));
        }
    }
    None
}

/// Is `line` purely hex digits and whitespace?
fn is_hex_text(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_hexdigit() || c.is_whitespace())
}

/// Hex digits of `s` decoded to bytes; `None` for an odd digit count.
fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let nibbles: Vec<u8> = s
        .chars()
        .filter_map(|c| c.to_digit(16).map(|d| d as u8))
        .collect();
    if nibbles.len() % 2 != 0 {
        return None;
    }
    Some(nibbles.chunks(2).map(|p| (p[0] << 4) | p[1]).collect())
}

/// Lines the ELM prints that carry no data. A line of only non-printable bytes
/// is the noise a clone's UART emits while it powers up (before its banner);
/// it cannot hold hex data, so it is skipped rather than reported as garbage.
fn is_noise(line: &str) -> bool {
    line.is_empty()
        || line.chars().all(|c| !c.is_ascii_graphic())
        || line == "OK"
        || line.starts_with("SEARCHING")
        || (line.starts_with("BUS INIT") && !line.contains("ERROR"))
}

/// An ISO-TP multi-frame reply being assembled from `N:` lines.
struct Group {
    total: usize,
    data: Vec<u8>,
    next_index: u8,
}

/// Parse one block of ELM327 text (everything before a `>` prompt, or what has
/// arrived so far) into its responses.
///
/// `sent_hex` is the request as written to the adapter (`22F190`). Its first
/// byte is the service a responsePending must echo to be waited on (see
/// [`diag::is_response_pending`](crate::protocol::diag::is_response_pending));
/// a `7F xx 78` for another service is a reply like any other.
///
/// Returns an error for any adapter error line (`NO DATA`, `CAN ERROR`,
/// `BUS BUSY`, `ERR94`, `?`, …), [`Error::AdapterReset`] when the adapter
/// reset (anywhere in the block) or echoed `sent_hex` back, and a protocol
/// error for a multi-frame reply cut short by another line or for text that is
/// not hex. Each line is its own response: single frames are never
/// concatenated across lines.
pub fn parse_block(text: &str, sent_hex: &str) -> Result<ElmBlock> {
    let mut out = ElmBlock::default();
    let mut group: Option<Group> = None;
    let req = hex_bytes(sent_hex).unwrap_or_default();
    let positive_sid = req.first().map(|s| s.wrapping_add(0x40));
    // The first non-pending response, and the first positive one for `req`.
    let mut first: Option<Vec<u8>> = None;
    let mut positive: Option<Vec<u8>> = None;

    let mut push = |resp: Vec<u8>, out: &mut ElmBlock| {
        if is_response_pending(&req, &resp) {
            out.pending += 1;
            return;
        }
        if positive.is_none() && positive_sid.is_some() && resp.first() == positive_sid.as_ref() {
            positive = Some(resp.clone());
        }
        if first.is_none() {
            first = Some(resp);
        }
    };

    // A reset banner anywhere in the block wins over any other line: whatever
    // came before it belongs to a session the adapter no longer has.
    for raw in text.split(['\r', '\n']) {
        let raw = raw.trim();
        if let Some(e) = reset_error(&raw.to_ascii_uppercase(), raw) {
            return Err(e);
        }
    }

    for raw in text.split(['\r', '\n']) {
        let raw = raw.trim();
        let line = raw.to_ascii_uppercase();
        if let Some(e) = line_error(&line, raw, sent_hex) {
            return Err(e);
        }
        if is_noise(&line) {
            continue;
        }

        // `N:` multi-frame continuation line.
        if let Some((idx, rest)) = line.split_once(':') {
            let idx = u8::from_str_radix(idx.trim(), 16)
                .map_err(|_| Error::Protocol(format!("malformed ELM multi-frame line: {raw}")))?;
            let g = group.as_mut().ok_or_else(|| {
                Error::Protocol(format!(
                    "ELM multi-frame line without a length header: {raw}"
                ))
            })?;
            if idx != g.next_index {
                return Err(Error::Protocol(format!(
                    "ELM multi-frame out of sequence: expected frame {:X}, got {raw} \
                     ({} of {} bytes received)",
                    g.next_index,
                    g.data.len(),
                    g.total
                )));
            }
            let bytes = if is_hex_text(rest) {
                hex_bytes(rest)
            } else {
                None
            }
            .ok_or_else(|| Error::Protocol(format!("malformed ELM multi-frame line: {raw}")))?;
            g.data.extend(bytes);
            g.next_index = (g.next_index + 1) & 0x0F;
            if g.data.len() >= g.total {
                let mut g = group.take().expect("group present");
                g.data.truncate(g.total);
                push(g.data, &mut out);
            }
            continue;
        }

        if !is_hex_text(&line) {
            return Err(Error::Protocol(format!("unexpected ELM reply: {raw}")));
        }
        // A new line while a multi-frame is still open means it was cut short.
        if let Some(g) = group.take() {
            return Err(truncated(g.total, g.data.len()));
        }
        let compact: String = line.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() == 3 {
            // Total-length header of an ISO-TP multi-frame reply.
            let total = usize::from_str_radix(&compact, 16)
                .map_err(|_| Error::Protocol(format!("bad ELM length line: {raw}")))?;
            if total == 0 {
                return Err(Error::Protocol(format!("bad ELM length line: {raw}")));
            }
            group = Some(Group {
                total,
                data: Vec::new(),
                next_index: 0,
            });
            continue;
        }
        let bytes = hex_bytes(&compact)
            .ok_or_else(|| Error::Protocol(format!("odd-length hex in ELM reply: {raw}")))?;
        push(bytes, &mut out);
    }

    out.response = positive.or(first);
    if let Some(g) = group {
        if out.response.is_none() {
            out.incomplete = Some((g.total, g.data.len()));
        }
    }
    Ok(out)
}

/// Does `text` end inside a line (printable text after the last line break)?
fn ends_mid_line(text: &str) -> bool {
    let tail = text.rsplit(['\r', '\n']).next().unwrap_or_default();
    !tail.trim().is_empty()
}

fn truncated(total: usize, got: usize) -> Error {
    Error::Protocol(format!(
        "truncated ISO-TP multi-frame reply: expected {total} bytes, got {got}"
    ))
}

/// Read and select the reply to a request that has just been written.
///
/// `read_byte` returns `Ok(Some(b))` for a byte, `Ok(None)` for an idle tick
/// (no data within the stream's short read timeout), or an I/O error.
///
/// Behaviour:
/// - A prompt block holding a non-pending response returns it (pending frames
///   before it are dropped; this is what a genuine ELM327 prints).
/// - A prompt block holding **only** responsePending frames (clones that return
///   to the prompt early) keeps listening for up to [`P2_STAR_MS`] *without*
///   expecting another prompt, and returns the first non-pending response line.
///   The request is never re-issued (and a bare `\r`, which repeats the last
///   command on an ELM, is never sent).
/// - Each responsePending (for the request's service) pushes the deadline out
///   to `p2_star_ms` from now. A `7F xx 78` for another service is returned as
///   the reply (the client reports it as mis-correlated), never waited on.
/// - A multi-frame reply that is still short at the prompt or deadline is an
///   error, never silently short data.
/// - A `>` in the middle of a line (text since the last line break) is stray
///   when more bytes follow at once: it is dropped and the line continues. If
///   the adapter goes idle after it, it was the prompt of a clone that omits
///   the final line break, and the block ends there.
pub fn read_reply(
    read_byte: &mut dyn FnMut() -> Result<Option<u8>>,
    sent_hex: &str,
    timeout_ms: u64,
    p2_star_ms: u64,
) -> Result<Vec<u8>> {
    let mut text = String::new();
    let mut deadline = Instant::now() + Duration::from_millis(timeout_ms);
    // responsePending frames seen in total, and within the current `text`.
    let mut pending_seen = 0usize;
    let mut text_pending = 0usize;
    // The adapter returned to the prompt after only responsePending frames.
    let mut after_prompt = false;
    // A `>` arrived mid-line; decided by the next read (data = stray).
    let mut prompt_mid_line = false;

    loop {
        let byte = read_byte()?;
        let prompt = match byte {
            Some(b'>') if ends_mid_line(&text) => {
                prompt_mid_line = true;
                false
            }
            Some(b'>') => true,
            // Idle right after a mid-line `>`: it was the prompt.
            None => std::mem::take(&mut prompt_mid_line),
            Some(_) => {
                // More text right after a mid-line `>`: it was stray.
                prompt_mid_line = false;
                false
            }
        };
        if prompt {
            let block = parse_block(&text, sent_hex)?;
            if let Some(resp) = block.response {
                return Ok(resp);
            }
            if let Some((total, got)) = block.incomplete {
                return Err(truncated(total, got));
            }
            if block.pending > 0 {
                after_prompt = true;
                pending_seen += block.pending.saturating_sub(text_pending);
                deadline = Instant::now() + Duration::from_millis(p2_star_ms);
            }
            // An empty block is a stray prompt (e.g. left over from an
            // earlier exchange): keep waiting for the real reply.
            text.clear();
            text_pending = 0;
        }
        match byte {
            Some(b'>') | None => {}
            Some(b) => {
                text.push(b as char);
                if b == b'\r' || b == b'\n' {
                    let block = parse_block(&text, sent_hex)?;
                    if block.pending > text_pending {
                        // The ECU asked for more time: extend to P2*.
                        pending_seen += block.pending - text_pending;
                        text_pending = block.pending;
                        deadline = Instant::now() + Duration::from_millis(p2_star_ms);
                    }
                    if after_prompt {
                        // No further prompt is guaranteed; take the first
                        // complete non-pending line as the answer.
                        if let Some(resp) = block.response {
                            return Ok(resp);
                        }
                    }
                }
            }
        }
        if pending_seen > MAX_PENDING {
            return Err(Error::Timeout(p2_star_ms));
        }
        if Instant::now() >= deadline {
            // Some clones omit the prompt: use what arrived, if complete.
            let block = parse_block(&text, sent_hex)?;
            if let Some(resp) = block.response {
                return Ok(resp);
            }
            if let Some((total, got)) = block.incomplete {
                return Err(truncated(total, got));
            }
            let waited = if pending_seen > 0 {
                p2_star_ms
            } else {
                timeout_ms
            };
            return Err(Error::Timeout(waited));
        }
    }
}

/// Is an `AT` command reply a rejection (`?`, `ERROR`)?
pub fn at_rejected(reply: &str) -> bool {
    let up = reply.to_ascii_uppercase();
    up.split(['\r', '\n']).any(|l| l.trim() == "?") || up.contains("ERROR")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn reader(script: &str) -> impl FnMut() -> Result<Option<u8>> {
        let mut q: VecDeque<u8> = script.bytes().collect();
        move || Ok(q.pop_front())
    }

    fn read(script: &str, sent: &str) -> Result<Vec<u8>> {
        let mut r = reader(script);
        read_reply(&mut r, sent, 200, 300)
    }

    #[test]
    fn single_frame() {
        assert_eq!(
            read("6EF1A0\r\r>", "2EF1A0").unwrap(),
            vec![0x6E, 0xF1, 0xA0]
        );
        assert_eq!(
            read("41 0C 1A F8 \r\r>", "010C").unwrap(),
            vec![0x41, 0x0C, 0x1A, 0xF8]
        );
    }

    #[test]
    fn genuine_elm_pending_and_final_in_one_prompt_block() {
        // A genuine ELM keeps listening after 7F xx 78 and prints both before
        // a single prompt.
        assert_eq!(
            read("7F2E78\r6EF1A0\r\r>", "2EF1A0112233").unwrap(),
            vec![0x6E, 0xF1, 0xA0]
        );
        assert_eq!(
            read("7F2E78\r7F2E78\r6EF1A0\r\r>", "2EF1A0").unwrap(),
            vec![0x6E, 0xF1, 0xA0]
        );
    }

    #[test]
    fn clone_pending_then_prompt_then_final_without_prompt() {
        // Clone returns to the prompt after the pending frame; the answer
        // arrives later with no further prompt.
        assert_eq!(
            read("7F3178\r\r>7101FF00\r", "3101FF00").unwrap(),
            vec![0x71, 0x01, 0xFF, 0x00]
        );
        // ... and with a trailing prompt too.
        assert_eq!(
            read("7F3178\r\r>7101FF00\r\r>", "3101FF00").unwrap(),
            vec![0x71, 0x01, 0xFF, 0x00]
        );
    }

    #[test]
    fn multiframe_after_pending() {
        let script = "7F2278\r014\r0:62F190575657\r1:5A5A5A314B5A41\r2:57303030303031\r\r>";
        let out = read(script, "22F190").unwrap();
        assert_eq!(out.len(), 20);
        assert_eq!(&out[..3], &[0x62, 0xF1, 0x90]);
        assert_eq!(&out[3..], b"WVWZZZ1KZAW000001");
    }

    fn read_bytes(script: &[u8], sent: &str) -> Result<Vec<u8>> {
        let mut q: VecDeque<u8> = script.iter().copied().collect();
        let mut r = move || Ok(q.pop_front());
        read_reply(&mut r, sent, 200, 300)
    }

    #[test]
    fn matching_response_pending_is_waited_on() {
        // `7F 31 78` answers the 0x31 request: counted, and the real reply
        // after it is returned.
        let b = parse_block("7F3178\r", "3101FF00").unwrap();
        assert_eq!((b.pending, b.response), (1, None));
        assert_eq!(
            read("7F3178\r\r>7101FF00\r", "3101FF00").unwrap(),
            vec![0x71, 0x01, 0xFF, 0x00]
        );
    }

    #[test]
    fn mismatched_response_pending_is_handed_back() {
        // `7F 22 78` echoes another service than the 0x31 request: it is not
        // a reason to wait, it is the reply (the client then reports it as
        // mis-correlated), like `diag::is_response_pending` on every driver.
        let b = parse_block("7F2278\r", "3101FF00").unwrap();
        assert_eq!(b.pending, 0);
        assert_eq!(b.response, Some(vec![0x7F, 0x22, 0x78]));
        let start = Instant::now();
        assert_eq!(
            read("7F2278\r\r>", "3101FF00").unwrap(),
            vec![0x7F, 0x22, 0x78]
        );
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "not waited on"
        );
        // After a matching pending (clone back at the prompt), a mismatched
        // one is the reply too, not another extension.
        assert_eq!(
            read("7F3178\r\r>7F2278\r", "3101FF00").unwrap(),
            vec![0x7F, 0x22, 0x78]
        );
        // With no request to compare against, nothing is pending.
        assert_eq!(parse_block("7F3178\r", "").unwrap().pending, 0);
    }

    #[test]
    fn positive_reply_wins_over_another_ecus_negative_one() {
        // Functional request answered by two ECUs (headers off, so they cannot
        // be told apart): the positive reply for the service is the answer.
        let b = parse_block("7F0112\r410D64\r", "010D").unwrap();
        assert_eq!(b.response, Some(vec![0x41, 0x0D, 0x64]));
        let b = parse_block("7F2278\r7101FF00\r", "3101FF00").unwrap();
        assert_eq!(b.response, Some(vec![0x71, 0x01, 0xFF, 0x00]));
        // Only negatives: the first one, never a concatenation.
        let b = parse_block("7F0D12\r7F0D31\r", "0D").unwrap();
        assert_eq!(b.response, Some(vec![0x7F, 0x0D, 0x12]));
        // Two multi-frame answers: the first complete one.
        let two = "00A\r0:62F19031323334\r1:35363738\r00A\r0:62F19041424344\r1:45464748\r";
        let b = parse_block(two, "22F190").unwrap();
        assert_eq!(&b.response.unwrap()[3..], b"1234567");
        // A single frame from another ECU inside a multi-frame is an error,
        // never spliced data.
        assert!(parse_block("00A\r0:62F19031323334\r410D64\r1:35363738\r", "22F190").is_err());
    }

    #[test]
    fn searching_is_skipped_and_its_failure_is_an_error() {
        assert_eq!(
            read("SEARCHING...\r410D64\r\r>", "010D").unwrap(),
            vec![0x41, 0x0D, 0x64]
        );
        let e = read("SEARCHING...\rUNABLE TO CONNECT\r\r>", "010D").unwrap_err();
        assert!(matches!(e, Error::Protocol(_)), "{e:?}");
    }

    #[test]
    fn adapter_error_lines_end_the_read_at_once() {
        // Each of these is the whole answer: an error, not a wait for P2*.
        for t in ["STOPPED", "BUFFER FULL", "CAN ERROR", "NO DATA", "?"] {
            let start = Instant::now();
            let e = read(&format!("{t}\r\r>"), "22F190").unwrap_err();
            assert!(matches!(e, Error::Protocol(_)), "{t}: {e:?}");
            assert!(start.elapsed() < Duration::from_millis(150), "{t}");
        }
        // BUFFER FULL in the middle of a multi-frame reply: an error, not a
        // short image.
        let e = read("014\r0:62F19031323334\rBUFFER FULL\r\r>", "22F190").unwrap_err();
        assert!(
            matches!(&e, Error::Protocol(m) if m.contains("BUFFER FULL")),
            "{e:?}"
        );
    }

    #[test]
    fn stray_prompt_mid_line_is_dropped() {
        // A `>` inside a line with more text right behind it is stray.
        assert_eq!(
            read("410D>64\r\r>", "010D").unwrap(),
            vec![0x41, 0x0D, 0x64]
        );
        assert_eq!(
            read(
                "014\r0:62F190575657\r1:5A5A5A>314B5A41\r2:57303030303031\r\r>",
                "22F190"
            )
            .unwrap()
            .len(),
            20
        );
        // A `>` straight after the data with the adapter idle afterwards is
        // the prompt of a clone that skips the final line break.
        let start = Instant::now();
        assert_eq!(read("410D64>", "010D").unwrap(), vec![0x41, 0x0D, 0x64]);
        assert!(start.elapsed() < Duration::from_millis(150));
        let e = read("NO DATA>", "010D").unwrap_err();
        assert!(matches!(e, Error::Protocol(_)), "{e:?}");
    }

    #[test]
    fn power_cycle_mid_session_is_adapter_reset() {
        // Power-up noise from the clone's UART, then its banner, arriving
        // where the reply to a request was expected.
        let mut script = vec![0x00, 0xFF, b'\r', 0xFE, b'\r'];
        script.extend_from_slice(b"\rELM327 v1.5\r\r>");
        let e = read_bytes(&script, "010C").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
        // ... in the middle of a multi-frame reply.
        let e = read("014\r0:62F19031323334\r\r\rELM327 v1.5\r\r>", "22F190").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
        // A banner anywhere in a block wins over an earlier error line.
        let e = parse_block("STOPPED\r\rELM327 v1.5\r", "010C").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
        // The banner was missed (drained) and the next request meets the
        // rebooted defaults: echo on, spaces on, protocol search.
        let e = read("010C\rSEARCHING...\r41 0C 1A F8 \r\r>", "010C").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
    }

    #[test]
    fn only_pending_times_out_after_p2_star() {
        let start = Instant::now();
        let err = read("7F3178\r\r>", "3101").unwrap_err();
        assert!(matches!(err, Error::Timeout(300)), "{err:?}");
        assert!(start.elapsed() >= Duration::from_millis(250));
    }

    #[test]
    fn stray_prompt_is_ignored() {
        assert_eq!(
            read(">410D64\r\r>", "010D").unwrap(),
            vec![0x41, 0x0D, 0x64]
        );
    }

    #[test]
    fn truncated_multiframe_is_an_error() {
        // Length says 0x14 = 20 bytes but only 10 arrived before the prompt.
        let err = read("014\r0:62F19031323334\r1:353637\r\r>", "22F190").unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("truncated")),
            "{err:?}"
        );
        // ... and when cut off by the deadline with no prompt.
        let err = read("014\r0:62F19031323334\r", "22F190").unwrap_err();
        assert!(
            matches!(&err, Error::Protocol(m) if m.contains("truncated")),
            "{err:?}"
        );
        // A missing frame index is caught too.
        assert!(parse_block("014\r0:62F19031323334\r2:35363738393031\r", "").is_err());
    }

    #[test]
    fn multiple_ecus_are_not_concatenated() {
        let b = parse_block("410D64\r410D00\r", "010D").unwrap();
        assert_eq!(b.response, Some(vec![0x41, 0x0D, 0x64]));
    }

    #[test]
    fn error_strings_are_errors_not_data() {
        for t in [
            "NO DATA",
            "CAN ERROR",
            "BUS BUSY",
            "BUS ERROR",
            "ACT ALERT",
            "LP ALERT",
            "ERR94",
            "?",
            "FB ERROR",
            "<RX ERROR",
            "7E8 10 14 <DATA ERROR",
            "BUFFER FULL",
            "STOPPED",
            "UNABLE TO CONNECT",
            "BUS INIT: ...ERROR",
        ] {
            let e = parse_block(&format!("{t}\r"), "0100").unwrap_err();
            assert!(matches!(e, Error::Protocol(_)), "{t}: {e:?}");
        }
        // Non-hex garbage is never parsed as data.
        assert!(parse_block("HELLO\r", "0100").is_err());
    }

    #[test]
    fn reset_and_echo_are_adapter_reset() {
        for t in ["LV RESET", "ELM327 v1.5", "\r\rELM327 v1.5\r"] {
            let e = parse_block(t, "0100").unwrap_err();
            assert!(matches!(e, Error::AdapterReset(_)), "{t}: {e:?}");
        }
        let e = parse_block("22F190\r62F19041\r", "22F190").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
        let e = read("22 F1 90\r62F19041\r\r>", "22F190").unwrap_err();
        assert!(matches!(e, Error::AdapterReset(_)), "{e:?}");
    }

    #[test]
    fn kline_init_banner_is_skipped() {
        assert_eq!(
            read("BUS INIT: ...OK\rC1EF8F\r\r>", "81").unwrap(),
            vec![0xC1, 0xEF, 0x8F]
        );
    }

    #[test]
    fn can_request_length_limit() {
        assert!(check_can_request_len(&[0x22, 0xF1, 0x90]).is_ok());
        assert!(check_can_request_len(&[0; 7]).is_ok());
        assert!(matches!(
            check_can_request_len(&[0; 8]),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn at_rejection() {
        assert!(at_rejected("?"));
        assert!(at_rejected("ATCRA 7E8\r?"));
        assert!(!at_rejected("OK"));
    }
}
