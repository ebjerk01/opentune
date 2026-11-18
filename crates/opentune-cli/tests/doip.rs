//! `opentune doip …` smoke tests through the library entry point, against the
//! simulated DoIP entity (`--sim`, loopback UDP + TCP on ephemeral ports).
use opentune_cli::{try_run_from, CliConfig, ExitStatus};

fn run(args: &[&str]) -> anyhow::Result<()> {
    let mut v = vec!["opentune", "doip"];
    v.extend_from_slice(args);
    try_run_from(CliConfig::default(), v)
}

fn exit_status(r: anyhow::Result<()>) -> Option<u8> {
    r.err()
        .and_then(|e| e.downcast_ref::<ExitStatus>().map(|s| s.0))
}

#[test]
fn discover_on_the_simulator() {
    run(&["discover", "--sim", "--timeout-ms", "1000"]).unwrap();
    run(&["discover", "--sim", "--json", "--timeout-ms", "1000"]).unwrap();
    run(&["discover", "--sim", "--vin", "WVWZZZ00000000000"]).unwrap();
    run(&["discover", "--sim", "--eid", "02:00:00:00:00:10"]).unwrap();
    run(&["discover", "--sim", "--protocol-version", "0xFF"]).unwrap();
}

#[test]
fn discover_then_activate_routing() {
    run(&["discover", "--sim", "--activate"]).unwrap();
    run(&["discover", "--sim", "--activate", "--json"]).unwrap();
}

#[test]
fn listen_for_announcements() {
    run(&["discover", "--sim", "--listen", "--timeout-ms", "300"]).unwrap();
}

#[test]
fn nothing_found_is_exit_status_1() {
    let r = run(&[
        "discover",
        "--sim",
        "--eid",
        "090909090909",
        "--timeout-ms",
        "100",
    ]);
    assert_eq!(exit_status(r), Some(1));
    let r = run(&[
        "discover",
        "--sim",
        "--sim-fault",
        "no-reply",
        "--timeout-ms",
        "100",
    ]);
    assert_eq!(exit_status(r), Some(1));
}

#[test]
fn status_and_power_on_the_simulator() {
    run(&["status", "--sim"]).unwrap();
    run(&["status", "--sim", "--json"]).unwrap();
    run(&["power", "--sim"]).unwrap();
    run(&["power", "--sim", "--json"]).unwrap();
}

#[test]
fn faults_surface_as_errors() {
    let e = run(&["power", "--sim", "--sim-fault", "nack:0x03"]).unwrap_err();
    assert!(format!("{e:#}").contains("outOfMemory"), "{e:#}");
    let e = run(&[
        "status",
        "--sim",
        "--sim-fault",
        "bad-version",
        "--timeout-ms",
        "200",
    ])
    .unwrap_err();
    assert!(format!("{e:#}").contains("malformed"), "{e:#}");
    assert!(run(&["power", "--sim", "--sim-fault", "bogus"]).is_err());
}

#[test]
fn status_needs_an_address() {
    let e = run(&["status"]).unwrap_err();
    assert!(format!("{e:#}").contains("--ip"), "{e:#}");
    // --sim and --ip are exclusive.
    assert!(run(&["power", "--sim", "--ip", "127.0.0.1"]).is_err());
    // A selector and --listen are exclusive.
    assert!(run(&["discover", "--listen", "--vin", "WVWZZZ00000000000"]).is_err());
}
