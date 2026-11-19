//! End to end: a user's definitions file + an external provider program,
//! through the `SeedKey` hook (`Interface::set_seed_key`) against the UDS
//! simulator's SecurityAccess, plus the provider failure modes.
//!
//! This test has its own `main` (`harness = false`): when started with the
//! marker variable set, the test binary *is* the synthetic provider program
//! (it speaks the documented JSON protocol). Its transforms are test-only
//! (`key = seed XOR Mask`, the simulator's own test relationship) and not any
//! manufacturer's algorithm. Nothing here is shipped.
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{ConnectConfig, Interface, MemoryRegion};
use opentune_seedkey_loader::{Loader, LoaderConfig, ProviderSpec, PROTOCOL};
use opentune_udssim::UdssimDriver;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MARK: &str = "OPENTUNE_SEEDKEY_LOADER_TEST_PROVIDER";

fn main() {
    if std::env::var_os(MARK).is_some() {
        provider_main();
        return;
    }
    // Children started by the loader inherit this and act as the provider.
    std::env::set_var(MARK, "1");
    let tests: &[(&str, fn())] = &[
        (
            "unlocks_udssim_through_the_hook",
            unlocks_udssim_through_the_hook,
        ),
        (
            "wrong_parameter_is_denied_by_the_ecu",
            wrong_parameter_is_denied_by_the_ecu,
        ),
        ("off_by_default", off_by_default),
        ("refused_entries_never_run", refused_entries_never_run),
        ("provider_timeout", provider_timeout),
        (
            "provider_garbage_and_failures",
            provider_garbage_and_failures,
        ),
    ];
    let mut failed = 0;
    for (name, f) in tests {
        let ok = std::panic::catch_unwind(*f).is_ok();
        println!("test {name} ... {}", if ok { "ok" } else { "FAILED" });
        failed += usize::from(!ok);
    }
    println!(
        "\ntest result: {}. {} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" },
        tests.len() - failed
    );
    if failed > 0 {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// The synthetic provider program
// ---------------------------------------------------------------------------

fn reply(v: Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
}

fn provider_main() {
    let mut raw = Vec::new();
    let _ = std::io::stdin().take(1 << 20).read_to_end(&mut raw);
    let req: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return reply(json!({"error": "bad request"})),
    };
    if req["protocol"] != PROTOCOL {
        return reply(json!({"error": "unknown protocol"}));
    }
    let hex = |s: &str| -> Vec<u8> {
        (0..s.len() / 2)
            .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap())
            .collect()
    };
    match req["provider"].as_str().unwrap_or_default() {
        "DemoXor" => {
            let seed = hex(req["seed"].as_str().unwrap());
            let mask = req["parameters"]
                .as_array()
                .and_then(|ps| ps.iter().find(|p| p["key"] == "Mask"))
                .filter(|p| p["type"] == "ByteArray")
                .map(|p| hex(p["value"].as_str().unwrap()));
            let Some(mask) = mask.filter(|m| !m.is_empty()) else {
                return reply(json!({"error": "DemoXor needs a ByteArray Mask"}));
            };
            if req["key_length"].as_u64() != Some(seed.len() as u64) {
                return reply(json!({"error": "DemoXor needs KeyLength == SeedLength"}));
            }
            let key: String = seed
                .iter()
                .zip(mask.iter().cycle())
                .map(|(s, m)| format!("{:02X}", s ^ m))
                .collect();
            reply(json!({ "key": key }));
        }
        "DemoSleep" => std::thread::sleep(Duration::from_secs(30)),
        "DemoGarbage" => print!("this is not json"),
        "DemoHuge" => {
            let chunk = vec![b'A'; 64 * 1024];
            let mut out = std::io::stdout().lock();
            for _ in 0..32 {
                if out.write_all(&chunk).is_err() {
                    break;
                }
            }
        }
        "DemoFail" => {
            reply(json!({"error": "synthetic failure"}));
            std::process::exit(3);
        }
        "DemoShortKey" => reply(json!({"key": "01"})),
        "DemoSilent" => {}
        _ => reply(json!({"error": "unknown provider"})),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Tmp(PathBuf);
impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> Tmp {
    let d = std::env::temp_dir().join(format!("ot-skl-e2e-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

/// A synthetic definitions file: the simulator ECU (level 0x11, 4-byte seed
/// and key) for each test provider, a mismatched mask, and a refused entry.
fn defs() -> String {
    let entry = |name: &str, provider: &str, mask: &str| {
        format!(
            r#"{{"EcuName":"{name}","Aliases":[],"AccessLevel":17,"SeedLength":4,"KeyLength":4,"Provider":"{provider}","Origin":"synthetic test","Parameters":[{{"Key":"Mask","Value":"{mask}","DataType":"ByteArray"}}]}}"#
        )
    };
    let list = [
        entry("SIM-ECU", "DemoXor", "A5"),
        entry("SIM-WRONG", "DemoXor", "5A"),
        entry("SIM-SLEEP", "DemoSleep", "00"),
        entry("SIM-GARBAGE", "DemoGarbage", "00"),
        entry("SIM-HUGE", "DemoHuge", "00"),
        entry("SIM-FAIL", "DemoFail", "00"),
        entry("SIM-SHORT", "DemoShortKey", "00"),
        entry("SIM-SILENT", "DemoSilent", "00"),
        entry("SIM-UNKNOWN", "NoSuchAlgo", "00"),
        entry("ESL-SIM", "DemoXor", "A5"),
    ];
    format!("[{}]", list.join(","))
}

fn loader(t: &Tmp, timeout: Duration) -> Loader {
    let f = t.0.join("defs.json");
    std::fs::write(&f, defs()).unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut cfg = LoaderConfig::new(f, vec![ProviderSpec::Program(exe)]);
    cfg.timeout = timeout;
    Loader::new(&cfg).unwrap()
}

fn handler(l: &Loader, name: &str) -> Box<dyn SeedKey> {
    Box::new(l.seed_key(&format!("unlockecu:{name}")).unwrap().unwrap())
}

fn open_sim(name: &str) -> UdssimDriver {
    let mut d = UdssimDriver::new();
    let mut cfg = ConnectConfig::default();
    cfg.options.insert("ecu".into(), name.into());
    cfg.options.insert("reset".into(), "1".into());
    d.open(&cfg).unwrap();
    d
}

fn payload() -> Vec<u8> {
    (0..0x100).map(|i| (i * 7 % 256) as u8).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn unlocks_udssim_through_the_hook() {
    let t = tmp("hook");
    let l = loader(&t, Duration::from_secs(10));
    let report = l.report().join("\n");
    assert!(report.contains("9 loaded, 1 refused"), "{report}");
    let mut d = open_sim("skl-e2e-hook");
    let region = MemoryRegion::new(0x3000, 0x100);
    let data = payload();
    // The registry-style path: a boxed handler through Interface::set_seed_key.
    let drv: &mut dyn Interface = &mut d;
    assert!(drv.set_seed_key(handler(&l, "SIM-ECU")));
    drv.write_memory(region, &data, &mut |_| {}).unwrap();
    assert_eq!(drv.read_memory(region, &mut |_| {}).unwrap(), data);
}

fn wrong_parameter_is_denied_by_the_ecu() {
    let t = tmp("wrong");
    let l = loader(&t, Duration::from_secs(10));
    let mut d = open_sim("skl-e2e-wrong");
    assert!(d.set_seed_key(handler(&l, "SIM-WRONG")));
    let e = d
        .write_memory(MemoryRegion::new(0x3000, 0x100), &payload(), &mut |_| {})
        .unwrap_err()
        .to_string();
    assert!(
        e.to_lowercase().contains("denied") || e.contains("0x35"),
        "{e}"
    );
}

fn off_by_default() {
    // No loader: the driver has no handler and refuses to program.
    let mut d = open_sim("skl-e2e-off");
    assert!(d
        .write_memory(MemoryRegion::new(0x3000, 0x100), &payload(), &mut |_| {})
        .is_err());
    // A definitions file alone does not switch the loader on.
    let t = tmp("off");
    let f = t.0.join("defs.json");
    std::fs::write(&f, defs()).unwrap();
    let e = Loader::new(&LoaderConfig::new(f, vec![])).unwrap_err();
    assert!(e.contains("stays off"), "{e}");
}

fn refused_entries_never_run() {
    let t = tmp("refused");
    let l = loader(&t, Duration::from_secs(10));
    let r = &l.definitions().refused;
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].ecu_name.as_str(), r[0].term), ("ESL-SIM", "esl"));
    let e = l.seed_key("unlockecu:ESL-SIM").unwrap().unwrap_err();
    assert!(e.contains("CONTRIBUTING.md"), "{e}");
}

fn provider_timeout() {
    let t = tmp("timeout");
    let l = loader(&t, Duration::from_millis(300));
    let start = Instant::now();
    let e = handler(&l, "SIM-SLEEP")
        .compute(&[1, 2, 3, 4], 0x11)
        .unwrap_err()
        .to_string();
    assert!(e.contains("did not answer within 300 ms"), "{e}");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "the program was not stopped"
    );
}

fn provider_garbage_and_failures() {
    let t = tmp("garbage");
    let l = loader(&t, Duration::from_secs(10));
    let seed = [0x11, 0x22, 0x33, 0x44];
    // The good path for comparison: seed XOR A5.
    assert_eq!(
        handler(&l, "SIM-ECU").compute(&seed, 0x11).unwrap(),
        [0xB4, 0x87, 0x96, 0xE1]
    );
    for (name, want) in [
        ("SIM-GARBAGE", "not a JSON object"),
        ("SIM-SILENT", "not a JSON object"),
        ("SIM-HUGE", "more than 65536 bytes"),
        ("SIM-FAIL", "failed (status 3): synthetic failure"),
        ("SIM-SHORT", "1-byte key"),
        ("SIM-UNKNOWN", "unknown provider"),
    ] {
        let e = handler(&l, name)
            .compute(&seed, 0x11)
            .unwrap_err()
            .to_string();
        assert!(e.contains(want), "{name}: {e}");
        assert!(!e.contains("AAAA"), "{name}: output echoed");
    }
}
