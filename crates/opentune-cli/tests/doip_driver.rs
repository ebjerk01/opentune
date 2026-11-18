//! `--driver doip` through the real `opentune` binary: the engines (coding,
//! adaptation, auto-scan, service, log, OBD inspection) and the device commands
//! reach the simulated DoIP entity on loopback. The address goes in `--port`
//! or `OPENTUNE_PORT` (the engine commands' only connection input), with the
//! driver's other options in its `?key=value` part.
use opentune_udssim::{DoIpSimStream, DtcRecord, TcpEntityServer, UdsServer};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

const ECU: u16 = 0x0010;

/// Placeholder engine data matching packs/vcar-demo's engine definitions.
fn engine_ecu() -> UdsServer {
    let mut s = UdsServer::new();
    s.set_did(0xF187, "OTDOIP-0001");
    s.set_did(0xF190, "WVWZZZ00000000000");
    s.set_did(0x0600, vec![3, 3, 0, 0, 0, 0]);
    s.set_did(0x0610, vec![0x03, 0x20]);
    s.set_did(0x0611, vec![30]);
    s.set_routine(0x0203, vec![0x00]);
    s.set_io(0x0500, vec![0x00]);
    s.add_dtc(DtcRecord::new([0x01, 0x01, 0x00], 0x09));
    s.set_memory(0x1000, (0..0x100u32).map(|i| i as u8).collect::<Vec<u8>>());
    s
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn pack(rel: &str) -> String {
    repo()
        .join("packs/vcar-demo")
        .join(rel)
        .to_string_lossy()
        .into_owned()
}

fn workdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("opentune-cli-doip-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Run `opentune <args>` with `OPENTUNE_PORT=<port>`; stdout on success.
fn run(port: &str, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
        .args(args)
        .env("OPENTUNE_PORT", port)
        .output()
        .expect("run opentune");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "opentune {args:?} failed\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
}

#[test]
fn every_engine_runs_over_doip() {
    let connections = Arc::new(AtomicUsize::new(0));
    let n = connections.clone();
    let entity = TcpEntityServer::spawn((Ipv4Addr::LOCALHOST, 0).into(), move || {
        n.fetch_add(1, Ordering::SeqCst);
        DoIpSimStream::new(engine_ecu(), ECU)
    })
    .unwrap();
    let port = format!("127.0.0.1:{}", entity.local_addr().port());
    let dir = workdir();

    // The registry lists it.
    assert!(run(&port, &["drivers"]).contains("doip"));

    // Device commands take --port.
    let out = run(&port, &["info", "--driver", "doip", "--port", &port]);
    assert!(out.contains("OTDOIP-0001"), "{out}");
    let out = run(
        &port,
        &[
            "read", "--driver", "doip", "--port", &port, "--addr", "0x1000", "--len", "0x40",
            "--plan",
        ],
    );
    assert!(out.contains("23 44 00 00 10 00 00 00 00 40"), "{out}");

    // Coding.
    let cod = pack("coding/engine.json");
    let out = run(
        &port,
        &["coding", "decode", "--def", &cod, "--driver", "doip"],
    );
    assert!(out.contains("diesel"), "{out}");

    // Adaptation.
    let adp = pack("adaptation/engine.json");
    let out = run(
        &port,
        &[
            "adaptation",
            "--def",
            &adp,
            "--driver",
            "doip",
            "read",
            "Idle speed target",
        ],
    );
    assert!(out.contains("raw=800"), "{out}");

    // Service function.
    let svc = pack("services/engine-fan-test.json");
    let out = run(
        &port,
        &[
            "service", "--script", &svc, "--driver", "doip", "run", "--ack",
        ],
    );
    assert!(!out.is_empty());

    // Auto-scan: a DoIP module map whose req_id is the logical address.
    let modules = dir.join("doip-modules.json");
    std::fs::write(
        &modules,
        r#"{"vehicle": "simulated DoIP entity", "modules": [
            {"name": "Engine", "addr": 16, "req_id": 16, "resp_id": 16, "protocol": "uds"},
            {"name": "Nobody", "addr": 66, "req_id": 66, "resp_id": 66, "protocol": "uds"}
        ]}"#,
    )
    .unwrap();
    let scan_port = format!("{port}?target=tx_id");
    let out = run(
        &scan_port,
        &[
            "autoscan",
            "--modules",
            modules.to_str().unwrap(),
            "--driver",
            "doip",
        ],
    );
    assert!(out.contains("1 present"), "{out}");
    assert!(out.contains("VIN=WVWZZZ00000000000"), "{out}");

    // Live logging of a UDS DID.
    let logger = dir.join("logger.xml");
    std::fs::write(
        &logger,
        r#"<logger><protocols><protocol id="UDS" baud="500000">
          <parameters mode="22">
            <parameter id="IDLE" name="Idle target" desc="DID 0610">
              <address length="2">0x0610</address>
              <conversions><conversion units="rpm" expr="x" format="0"/></conversions>
            </parameter>
          </parameters>
        </protocol></protocols></logger>"#,
    )
    .unwrap();
    let out = run(
        &port,
        &["log", "--def", logger.to_str().unwrap(), "--driver", "doip"],
    );
    assert!(out.contains("IDLE") && out.contains(",800,"), "{out}");

    // OBD inspection reaches the entity too (this UDS simulator has no
    // OBD-II modes, so each section reports the ECU's negative answer).
    let out = run(
        &port,
        &["obd", "dtcs", "--driver", "doip", "--port", &port, "--json"],
    );
    assert!(out.contains("ecus"), "{out}");

    assert!(
        connections.load(Ordering::SeqCst) >= 9,
        "every command opened its own DoIP connection"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
