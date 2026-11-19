//! `--driver openport2` through the real `opentune` binary, the real serial
//! code path (`serialport` opening a device node) and the real driver, against
//! the offline fake OpenPort 2.0 running on the far end of a pseudo-terminal,
//! with the independent `opentune-udssim` UDS server as the ECU: `info`,
//! `identify` and a block `read`. Offline only; hardware validation pending.
#![cfg(unix)]

use opentune_driver_openport2::fake::{serve, FakeConfig, FakeHandle, FakeOp2};
use opentune_udssim::UdsServer;
use serialport::{SerialPort, TTYPort};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const BASE: u32 = 0x0004_0000;

fn image() -> Vec<u8> {
    (0..0x400u32).map(|i| (i * 31 + (i >> 3)) as u8).collect()
}

struct Fake {
    path: String,
    /// Keeps the pty slave open between CLI runs. Each run opens the node
    /// exclusively (TIOCEXCL + flock) and clears that again when its port is
    /// dropped, so this handle must not take a lock of its own.
    _slave: TTYPort,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    handle: FakeHandle,
}

impl Fake {
    fn start() -> Self {
        let (mut master, slave) = TTYPort::pair().expect("pty pair");
        master
            .set_timeout(Duration::from_millis(5))
            .expect("pty timeout");
        let path = slave.name().expect("pty name");
        let mut ecu = UdsServer::new();
        ecu.set_did(0xF190, b"OPENPORTFAKE00002".to_vec());
        ecu.set_did(0xF197, b"FAKE ENGINE".to_vec());
        ecu.set_did(0xF194, b"SW-0001".to_vec());
        ecu.set_memory(BASE as u64, image());
        let (dev, handle) = FakeOp2::new(
            FakeConfig::default(),
            Box::new(move |req: &[u8]| ecu.respond(req).into_iter().collect()),
        );
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let thread = std::thread::spawn(move || serve(dev, master, s));
        Self {
            path,
            _slave: slave,
            stop,
            thread: Some(thread),
            handle,
        }
    }

    /// Run `opentune <args> --driver openport2 --port <pty>`; stdout on success.
    fn run(&mut self, args: &[&str]) -> String {
        let out = Command::new(env!("CARGO_BIN_EXE_opentune"))
            .args(args)
            .args(["--driver", "openport2", "--port", &self.path])
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
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[test]
fn info_identify_and_read_over_a_fake_openport() {
    let mut fake = Fake::start();

    let out = fake.run(&["info"]);
    assert!(out.contains("Tactrix OpenPort 2.0"), "{out}");
    assert!(out.contains("Battery   : 12.48 V"), "{out}");
    assert!(out.contains("VIN       : OPENPORTFAKE00002"), "{out}");

    let out = fake.run(&["identify", "--ecudefs", "/nonexistent-ecudefs"]);
    assert!(out.contains("ECU name : FAKE ENGINE"), "{out}");
    assert!(out.contains("SW number: SW-0001"), "{out}");

    let dir = std::env::temp_dir().join(format!("opentune-op2-fake-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("read.bin");
    let bin_s = bin.to_string_lossy().into_owned();
    let out = fake.run(&[
        "read", "--addr", "0x40100", "--len", "0x200", "--out", &bin_s,
    ]);
    assert!(out.contains("Read 512 bytes"), "{out}");
    assert_eq!(std::fs::read(&bin).unwrap(), image()[0x100..0x300]);
    let _ = std::fs::remove_dir_all(&dir);

    // Three sessions, each opened and closed in the documented order.
    let log = fake.handle.log();
    let opens = log.lines.iter().filter(|l| l.starts_with("ato5 ")).count();
    let closes = log.lines.iter().filter(|l| l.starts_with("atc5 ")).count();
    assert_eq!((opens, closes), (3, 3), "{:?}", log.lines);
    assert!(log.lines.iter().all(|l| !l.starts_with("atx")));
    assert!(log.open_channels.is_empty(), "the cable is released");
}
