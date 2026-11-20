//! The Debug panel's operations, gated on the build channel HERE (not only
//! by the view's Bench registration): every entry point returns `Err`
//! before touching an adapter or a socket unless the channel is alpha /
//! bench.
//!
//! * [`raw_request`]: one raw diagnostic request to the app's target module
//!   through `AdapterAccess::read`, which admits only `READ_SERVICES`; the
//!   request is also checked here first, so a refusal says why at once.
//!   ECU-changing requests (`2E`, `31`, `34`, `14`, `2F`, `11`, ...) are
//!   refused unsent: this panel has no write path. ECU changes go through a
//!   module panel's `Host::confirm_ecu` + `mutate` (with a backup) or
//!   `actuate`.
//! * [`simulate`]: the offline UDS conformance server (`opentune-udssim`), no
//!   hardware; it may answer writes, since it is a simulation.

use crate::host::{bench_channel, AdapterAccess, BuildChannel, DiagTarget, READ_SERVICES};
use crate::ops::refuse_ecu_changing;
use std::time::Instant;

/// `Err` unless the channel enables the Bench tier.
pub fn gate(channel: &BuildChannel) -> Result<(), String> {
    if bench_channel(channel) {
        Ok(())
    } else {
        Err(format!(
            "debug tools are available only in alpha builds (this is the '{}' channel)",
            channel.as_str()
        ))
    }
}

/// Hex bytes from text: `22 F1 90`, `22F190`, `0x22,0xF1,0x90`.
pub fn parse_hex(s: &str) -> Result<Vec<u8>, String> {
    let mut digits = String::new();
    for tok in s.split(|c: char| c.is_whitespace() || c == ',' || c == ':') {
        let tok = tok
            .strip_prefix("0x")
            .or_else(|| tok.strip_prefix("0X"))
            .unwrap_or(tok);
        // A lone "0x"-less single digit in a spaced list is one byte (`1` = 01).
        if tok.len() == 1 && s.contains(char::is_whitespace) {
            digits.push('0');
        }
        digits.push_str(tok);
    }
    if digits.is_empty() {
        return Err("no bytes".into());
    }
    if let Some(c) = digits.chars().find(|c| !c.is_ascii_hexdigit()) {
        return Err(format!("'{c}' is not a hex digit"));
    }
    if !digits.len().is_multiple_of(2) {
        return Err("odd number of hex digits".into());
    }
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

/// `22 F1 90`.
pub fn hexs(b: &[u8]) -> String {
    b.iter()
        .map(|x| format!("{x:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The read services this panel sends, for its note.
pub fn read_services() -> String {
    hexs(READ_SERVICES)
}

/// One raw READ-ONLY request to `target` (Bench tier). The exchange goes to
/// the session log either way.
pub fn raw_request(
    channel: &BuildChannel,
    adapter: &(dyn AdapterAccess + 'static),
    target: DiagTarget,
    req: Vec<u8>,
) -> Result<Vec<u8>, String> {
    gate(channel)?;
    let to = target.to_string();
    if let Err(e) = refuse_ecu_changing(&req) {
        super::session::push(format!("[{to}] -> {}  REFUSED (not sent)", hexs(&req)));
        return Err(e);
    }
    let line = hexs(&req);
    let t0 = Instant::now();
    let r = adapter.read("debug raw request", target, move |d| d.request(&req));
    match &r {
        Ok(resp) => super::session::push(format!(
            "[{to}] -> {line}  <- {}  ({} ms)",
            hexs(resp),
            t0.elapsed().as_millis()
        )),
        Err(e) => super::session::push(format!("[{to}] -> {line}  ERR {e}")),
    }
    r
}

/// Simulator exchanges: (request, response).
pub type Exchanges = Vec<(Vec<u8>, Vec<u8>)>;

/// Run `requests` (one hex request per line) against the offline UDS
/// server, seeded with `dids` (`F190=57565A...` per line). No hardware.
pub fn simulate(
    channel: &BuildChannel,
    dids: &str,
    requests: &str,
    require_security_for_write: bool,
) -> Result<Exchanges, String> {
    use opentune_udssim::server::{ServerConfig, UdsServer};
    gate(channel)?;
    let mut server = UdsServer::with_config(ServerConfig {
        require_security_for_write,
        ..Default::default()
    });
    for d in dids.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (id, hx) = d
            .split_once('=')
            .ok_or_else(|| format!("expected ID=HEX, got '{d}'"))?;
        let id = id.trim();
        let id = id
            .strip_prefix("0x")
            .or_else(|| id.strip_prefix("0X"))
            .unwrap_or(id);
        let did = u16::from_str_radix(id, 16).map_err(|_| format!("invalid DID '{id}'"))?;
        server.set_did(did, parse_hex(hx)?);
    }
    let mut out = Vec::new();
    for r in requests.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let req = parse_hex(r)?;
        let resp = server.handle(&req);
        super::session::push(format!("sim -> {}  <- {}", hexs(&req), hexs(&resp)));
        out.push((req, resp));
    }
    if out.is_empty() {
        return Err("no requests".into());
    }
    Ok(out)
}

#[cfg(test)]
pub(crate) mod testing {
    //! A recording adapter for the Debug tests (reads only).
    use crate::host::{Ack, AdapterAccess, BackupReceipt, DiagJob, DiagTarget, LinkInfo};
    use std::any::Any;
    use std::sync::{Arc, Mutex};

    /// Answers every request with `reply(req)` and records what reached it.
    pub struct Recorder {
        pub sent: Arc<Mutex<Vec<Vec<u8>>>>,
        pub reply: fn(&[u8]) -> Vec<u8>,
    }

    impl Recorder {
        pub fn new(reply: fn(&[u8]) -> Vec<u8>) -> Self {
            Self {
                sent: Arc::default(),
                reply,
            }
        }
        pub fn sent(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }
    }

    struct Wire(Arc<Mutex<Vec<Vec<u8>>>>, fn(&[u8]) -> Vec<u8>);
    impl opentune_core::RawDiag for Wire {
        fn request(&mut self, req: &[u8]) -> Result<Vec<u8>, String> {
            self.0.lock().unwrap().push(req.to_vec());
            Ok((self.1)(req))
        }
    }

    impl AdapterAccess for Recorder {
        fn link(&self) -> LinkInfo {
            LinkInfo {
                driver: "mock".into(),
                connected: true,
                battery_v: Some(12.6),
                ..Default::default()
            }
        }
        fn read_job(
            &self,
            _op: &str,
            _target: DiagTarget,
            job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            job(&mut Wire(self.sent.clone(), self.reply))
        }
        fn mutate_job(
            &self,
            _op: &str,
            _ack: Ack,
            _backup: &BackupReceipt,
            _job: DiagJob,
        ) -> Result<Box<dyn Any + Send>, String> {
            panic!("the Debug panel never mutates")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::Recorder;
    use super::*;

    fn alpha() -> BuildChannel {
        BuildChannel::new(BuildChannel::ALPHA)
    }

    fn echo(req: &[u8]) -> Vec<u8> {
        let mut r = vec![req[0] + 0x40];
        r.extend_from_slice(&req[1..]);
        r
    }

    #[test]
    fn hex_parsing() {
        assert_eq!(parse_hex("22 F1 90").unwrap(), [0x22, 0xF1, 0x90]);
        assert_eq!(parse_hex("22f190").unwrap(), [0x22, 0xF1, 0x90]);
        assert_eq!(parse_hex("0x22,0xF1, 0x90").unwrap(), [0x22, 0xF1, 0x90]);
        assert_eq!(parse_hex("10 3").unwrap(), [0x10, 0x03]);
        assert!(parse_hex("").is_err());
        assert!(parse_hex("2").is_err());
        assert!(parse_hex("zz").is_err());
        assert_eq!(hexs(&[0x7F, 0x22, 0x31]), "7F 22 31");
        assert!(read_services().starts_with("10 27 22"));
    }

    #[test]
    fn stable_refuses_before_the_adapter() {
        let a = Recorder::new(echo);
        let stable = BuildChannel::new(BuildChannel::STABLE);
        let e = raw_request(&stable, &a, DiagTarget::ENGINE, vec![0x22, 0xF1, 0x90]).unwrap_err();
        assert!(e.contains("alpha"), "{e}");
        assert!(simulate(&stable, "", "3E 00", false).is_err());
        assert!(a.sent().is_empty(), "nothing reached the adapter");
    }

    #[test]
    fn reads_pass_and_writes_are_refused_unsent() {
        let a = Recorder::new(echo);
        let r = raw_request(&alpha(), &a, DiagTarget::ENGINE, vec![0x22, 0xF1, 0x90]).unwrap();
        assert_eq!(r, [0x62, 0xF1, 0x90]);
        for write in [
            &[0x2E, 0xF1, 0x90, 0x00][..],
            &[0x31, 0x01, 0xFF, 0x00],
            &[0x34, 0x00, 0x44],
            &[0x14, 0xFF, 0xFF, 0xFF],
            &[0x2F, 0x01, 0x00, 0x03],
            &[0x11, 0x01],
            &[0x04],
        ] {
            let e = raw_request(&alpha(), &a, DiagTarget::ENGINE, write.to_vec()).unwrap_err();
            assert!(e.contains("refused"), "{e}");
        }
        assert_eq!(a.sent(), [vec![0x22, 0xF1, 0x90]], "only the read was sent");
        assert!(super::super::session::lines()
            .iter()
            .any(|l| l.contains("-> 2E F1 90 00  REFUSED (not sent)")));
    }

    #[test]
    fn simulator_answers_offline() {
        let out = simulate(
            &alpha(),
            "F190=57 56 57 5A 5A 5A\n",
            "22 F1 90\n\n22 F1 91\n2E F1 90 41",
            true,
        )
        .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out[0].1,
            [0x62, 0xF1, 0x90, 0x57, 0x56, 0x57, 0x5A, 0x5A, 0x5A]
        );
        assert_eq!(out[1].1[0], 0x7F, "unknown DID: negative response");
        assert_eq!(out[2].1[0], 0x7F, "write needs security access");
        assert!(simulate(&alpha(), "F190", "22 F1 90", false).is_err());
        assert!(simulate(&alpha(), "", "", false).is_err());
    }
}
