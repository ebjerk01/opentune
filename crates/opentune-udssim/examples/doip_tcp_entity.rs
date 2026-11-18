//! A TCP bridge in front of [`DoIpSimStream`]: the simulated ECU as a DoIP
//! entity on `127.0.0.1`, so an external DoIP/UDS client (the python-udsoncan +
//! python-doipclient conformance harness in `tools/conformance/`) can drive it.
//!
//! Offline conformance only: it binds the loopback interface and serves one
//! connection at a time, each against a fresh simulated ECU.
//!
//! ```text
//! cargo run -p opentune-udssim --example doip_tcp_entity -- [options]
//!   --port N                 TCP port (0 = ephemeral; default 0)
//!   --ecu 0xADDR             entity logical address (default 0x0010)
//!   --pending SID:COUNT      interpose COUNT responsePending before SID's answer
//!   --nrc SID:NRC            answer the next SID request with NRC
//!   --restrict SID:S1,S2     accept SID only in sessions S1, S2 (else 0x7F)
//!   --max-attempts N         SecurityAccess attempts before 0x36 (default 3)
//!   --sec-delay-ms N         SecurityAccess lockout after 0x36 (0x37 meanwhile)
//!   --secure-write           WriteDataByIdentifier needs SecurityAccess
//!   --secure-transfer        RequestDownload/Upload need SecurityAccess
//!   --flash                  enable the flashable-ECU model (2x 0x78 on erase)
//!   --alive-check            send an alive check request after routing activation
//!   --no-pos-ack             no diagnostic positive ack (0x8002)
//! ```
//!
//! Prints `LISTENING <port>` once bound, and one line per notable event
//! (`ALIVE_CHECK_RESPONSE 0x0E00`, `CLOSED`), all flushed, on stdout.

use opentune_core::protocol::DoIpStream;
use opentune_udssim::{DoIpSimStream, FlashConfig, ServerConfig, UdsServer};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

#[derive(Default)]
struct Opts {
    port: u16,
    ecu: u16,
    pending: Vec<(u8, usize)>,
    nrc: Vec<(u8, u8)>,
    restrict: Vec<(u8, Vec<u8>)>,
    max_attempts: Option<u32>,
    sec_delay_ms: Option<u64>,
    secure_write: bool,
    secure_transfer: bool,
    flash: bool,
    alive_check: bool,
    no_pos_ack: bool,
}

fn num(s: &str) -> u64 {
    let s = s.trim();
    let v = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16),
        None => s.parse(),
    };
    v.unwrap_or_else(|_| panic!("bad number {s:?}"))
}

fn pair(s: &str) -> (u64, &str) {
    let (a, b) = s
        .split_once(':')
        .unwrap_or_else(|| panic!("expected A:B, got {s:?}"));
    (num(a), b)
}

fn parse() -> Opts {
    let mut o = Opts {
        ecu: 0x0010,
        ..Opts::default()
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--port" => o.port = num(&val()) as u16,
            "--ecu" => o.ecu = num(&val()) as u16,
            "--pending" => {
                let v = val();
                let (sid, n) = pair(&v);
                o.pending.push((sid as u8, num(n) as usize));
            }
            "--nrc" => {
                let v = val();
                let (sid, n) = pair(&v);
                o.nrc.push((sid as u8, num(n) as u8));
            }
            "--restrict" => {
                let v = val();
                let (sid, list) = pair(&v);
                let sessions = list.split(',').map(|s| num(s) as u8).collect();
                o.restrict.push((sid as u8, sessions));
            }
            "--max-attempts" => o.max_attempts = Some(num(&val()) as u32),
            "--sec-delay-ms" => o.sec_delay_ms = Some(num(&val())),
            "--secure-write" => o.secure_write = true,
            "--secure-transfer" => o.secure_transfer = true,
            "--flash" => o.flash = true,
            "--alive-check" => o.alive_check = true,
            "--no-pos-ack" => o.no_pos_ack = true,
            other => panic!("unknown option {other}"),
        }
    }
    o
}

/// The simulated ECU every connection starts from. Placeholder data only.
fn ecu(o: &Opts) -> UdsServer {
    let mut cfg = ServerConfig {
        require_security_for_write: o.secure_write,
        require_security_for_transfer: o.secure_transfer,
        ..ServerConfig::default()
    };
    if let Some(n) = o.max_attempts {
        cfg.max_sec_attempts = n;
    }
    let mut s = UdsServer::with_config(cfg);
    s.set_did(0xF190, b"WVWZZZ00000000000".to_vec());
    s.set_did(0xF18C, b"SIM0001".to_vec());
    s.set_did(0x1000, vec![0x10, 0x20, 0x30, 0x40]);
    s.set_routine(0x0203, vec![0x00]);
    s.set_memory(0x0000_2000, (0u8..64).collect::<Vec<u8>>());
    s.set_io(0x0101, vec![0x00]);
    for (sid, sessions) in &o.restrict {
        s.restrict_service_to_sessions(*sid, sessions);
    }
    if let Some(ms) = o.sec_delay_ms {
        s.set_security_delay(Duration::from_millis(ms));
    }
    if o.flash {
        s.enable_flash(FlashConfig {
            erase_pending: 2,
            ..FlashConfig::default()
        });
    }
    s
}

fn serve(mut sock: TcpStream, o: &Opts) -> std::io::Result<()> {
    let mut entity = DoIpSimStream::new(ecu(o), o.ecu);
    entity.set_pos_ack(!o.no_pos_ack);
    for &(sid, n) in &o.pending {
        entity.faults().response_pending(sid, n);
    }
    for &(sid, code) in &o.nrc {
        entity.faults().negative_response(sid, code);
    }
    sock.set_nodelay(true)?;
    sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let mut alive_sent = false;
    let mut seen_alive = 0;
    let mut buf = [0u8; 4096];
    loop {
        match sock.read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                // The entity never fails a write; framing errors become NACKs.
                let _ = entity.write_all(&buf[..n]);
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(e),
        }
        if o.alive_check && !alive_sent && entity.routed_source().is_some() {
            entity.request_alive_check();
            alive_sent = true;
        }
        let out = entity.take_outbound();
        if !out.is_empty() {
            Write::write_all(&mut sock, &out)?;
        }
        let responses = entity.alive_check_responses();
        for addr in &responses[seen_alive..] {
            println!("ALIVE_CHECK_RESPONSE {addr:#06X}");
        }
        seen_alive = responses.len();
        std::io::stdout().flush()?;
    }
}

fn main() -> std::io::Result<()> {
    let o = parse();
    let listener = TcpListener::bind(("127.0.0.1", o.port))?;
    println!("LISTENING {}", listener.local_addr()?.port());
    std::io::stdout().flush()?;
    for conn in listener.incoming() {
        let r = serve(conn?, &o);
        println!("CLOSED {}", if r.is_ok() { "ok" } else { "error" });
        std::io::stdout().flush()?;
    }
    Ok(())
}
