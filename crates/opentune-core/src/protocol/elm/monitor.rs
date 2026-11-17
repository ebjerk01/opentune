//! `ATMA` (monitor all) output parsing, shared by the ELM327 bus monitors.
//!
//! With headers on (`ATH1`) and CAN auto-formatting off (`ATCAF0`) an ELM327
//! in monitor mode prints one raw frame per line, either spaced (`ATS1`:
//! `7E8 03 41 0C 1A`, `18 DA F1 10 03 41 0D`, optionally with the `ATD1` DLC
//! digit after the id) or compact (`ATS0`: `7E803410C1A`, `18DAF11003410D`).
//! Compact lines are unambiguous with `ATD0`: an 11-bit id is 3 hex digits and
//! the payload an even number, so the line length is odd; a 29-bit id is 8
//! digits and the length even. Overflow and error lines are classified too.

/// One classified `ATMA` output line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorLine {
    /// A received frame: (id, extended, rtr, data).
    Frame(u32, bool, bool, Vec<u8>),
    /// `BUFFER FULL`: the ELM overflowed and stopped monitoring.
    BufferFull,
    /// A damaged frame (`<RX ERROR`, `<DATA ERROR`, unparsable hex).
    Damaged,
    /// `CAN ERROR` / `BUS ERROR` / `BUS BUSY`: wrong bitrate, no bus, or the
    /// HS/MS switch in the wrong position.
    BusError,
    /// Anything else (`OK`, `STOPPED`, `SEARCHING...`, empty).
    Other,
}

fn hex_pairs(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Classify and parse one `ATMA` line, spaced (`7E8 8 03 41 0C ...`,
/// `18 DA F1 10 8 ...`, with or without the `ATD1` DLC digit) or compact
/// (`7E803410C...` / `18DAF11003410D...`, `ATS0`+`ATD0`).
pub fn parse_monitor_line(line: &str) -> MonitorLine {
    let line = line.trim();
    let up = line.to_ascii_uppercase();
    if up.is_empty() {
        return MonitorLine::Other;
    }
    if up.contains("BUFFER FULL") {
        return MonitorLine::BufferFull;
    }
    if up.contains("RX ERROR") || up.contains("DATA ERROR") || up.starts_with('<') {
        return MonitorLine::Damaged;
    }
    if up.contains("CAN ERROR") || up.contains("BUS ERROR") || up.contains("BUS BUSY") {
        return MonitorLine::BusError;
    }
    if !up.as_bytes()[0].is_ascii_hexdigit()
        || matches!(up.as_str(), "OK" | "STOPPED" | "NO DATA" | "?")
    {
        return MonitorLine::Other;
    }
    let (body, rtr) = match up.strip_suffix("RTR") {
        Some(b) => (b.trim_end(), true),
        None => (up.as_str(), false),
    };
    let tokens: Vec<&str> = body.split_whitespace().collect();
    let parsed = if tokens.len() == 1 {
        // Compact: odd length = 3-digit id, even = 8-digit id.
        let t = tokens[0];
        let id_len = if t.len() % 2 == 0 { 8 } else { 3 };
        if t.len() < id_len {
            None
        } else {
            let id = u32::from_str_radix(&t[..id_len], 16).ok();
            id.zip(hex_pairs(&t[id_len..]))
                .map(|(id, d)| (id, id_len == 8, d))
        }
    } else {
        let (id, ext, rest) = if tokens[0].len() == 3 {
            (u32::from_str_radix(tokens[0], 16).ok(), false, &tokens[1..])
        } else if tokens.len() >= 4 && tokens[..4].iter().all(|t| t.len() == 2) {
            (
                u32::from_str_radix(&tokens[..4].concat(), 16).ok(),
                true,
                &tokens[4..],
            )
        } else {
            (None, false, &tokens[..])
        };
        // Optional single-digit DLC (ATD1).
        let (dlc, rest) = match rest.first() {
            Some(t) if t.len() == 1 => (u8::from_str_radix(t, 16).ok(), &rest[1..]),
            _ => (None, rest),
        };
        let data: Option<Vec<u8>> = rest
            .iter()
            .map(|t| hex_pairs(t))
            .collect::<Option<Vec<Vec<u8>>>>()
            .map(|v| v.concat());
        let dlc_ok = |len: usize| match dlc {
            None => true,
            Some(n) => rtr || n as usize == len,
        };
        match (id, data) {
            (Some(id), Some(d)) if dlc_ok(d.len()) => Some((id, ext, d)),
            _ => None,
        }
    };
    match parsed {
        Some((id, ext, data)) if data.len() <= 8 && id <= if ext { 0x1FFF_FFFF } else { 0x7FF } => {
            MonitorLine::Frame(id, ext, rtr, if rtr { Vec::new() } else { data })
        }
        _ => MonitorLine::Damaged,
    }
}

/// Acceptance filter covering every id in `ids` (all the same width):
/// `(ATCF value, ATCM mask)` — the mask keeps only bits all ids agree on.
pub fn acceptance_filter(ids: &[u32]) -> Option<(u32, u32, bool)> {
    let first = *ids.first()?;
    let ext = first > 0x7FF;
    if ids.iter().any(|&i| (i > 0x7FF) != ext) {
        return None;
    }
    let full = if ext { 0x1FFF_FFFF } else { 0x7FF };
    let and = ids.iter().fold(full, |a, &i| a & i);
    let or = ids.iter().fold(0, |a, &i| a | i);
    let mask = !(and ^ or) & full;
    Some((and & mask, mask, ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_spaced_and_compact_lines() {
        use MonitorLine::*;
        let f = |id, ext, d: &[u8]| Frame(id, ext, false, d.to_vec());
        assert_eq!(
            parse_monitor_line("7E8 03 41 0C 1A"),
            f(0x7E8, false, &[0x03, 0x41, 0x0C, 0x1A])
        );
        assert_eq!(
            parse_monitor_line("7E8 4 03 41 0C 1A"),
            f(0x7E8, false, &[0x03, 0x41, 0x0C, 0x1A])
        );
        assert_eq!(
            parse_monitor_line("18 DA F1 10 3 03 41 0D"),
            f(0x18DA_F110, true, &[0x03, 0x41, 0x0D])
        );
        assert_eq!(
            parse_monitor_line("18 DA F1 10 03 41 0D"),
            f(0x18DA_F110, true, &[0x03, 0x41, 0x0D])
        );
        assert_eq!(parse_monitor_line("280"), f(0x280, false, &[]));
        assert_eq!(
            parse_monitor_line("7E803410C1A"),
            f(0x7E8, false, &[0x03, 0x41, 0x0C, 0x1A])
        );
        assert_eq!(
            parse_monitor_line("18DAF11003410D"),
            f(0x18DA_F110, true, &[0x03, 0x41, 0x0D])
        );
        assert_eq!(
            parse_monitor_line("123 RTR"),
            Frame(0x123, false, true, vec![])
        );
        assert_eq!(parse_monitor_line("BUFFER FULL"), BufferFull);
        assert_eq!(parse_monitor_line("<RX ERROR"), Damaged);
        assert_eq!(parse_monitor_line("7E8 03 41 <DATA ERROR"), Damaged);
        assert_eq!(parse_monitor_line("7E8 5 03 41"), Damaged); // DLC mismatch
        assert_eq!(parse_monitor_line("CAN ERROR"), BusError);
        assert_eq!(parse_monitor_line("STOPPED"), Other);
        assert_eq!(parse_monitor_line("OK"), Other);
        assert_eq!(parse_monitor_line(""), Other);
    }

    #[test]
    fn acceptance_filter_mask() {
        assert_eq!(acceptance_filter(&[0x7E8]), Some((0x7E8, 0x7FF, false)));
        // 7E0 / 7E8 differ only in bit 3.
        assert_eq!(
            acceptance_filter(&[0x7E8, 0x7E0]),
            Some((0x7E0, 0x7F7, false))
        );
        assert_eq!(acceptance_filter(&[0x7E8, 0x18DA_F110]), None);
        assert_eq!(acceptance_filter(&[]), None);
    }
}
