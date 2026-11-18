//! The `doip` driver end to end against the `opentune-udssim` DoIP entity on
//! loopback (UDP discovery side + TCP data side): open by address and by
//! discovery, identity, block read with SecurityAccess, and the real engines
//! (coding, adaptation, auto-scan, service functions, live logging) through
//! `Interface::diag`, exactly as the apps drive any registry driver.
use opentune_core::protocol::seedkey::SeedKey;
use opentune_core::{ConnectConfig, CoreDiag, Error, Interface, MemoryRegion};
use opentune_driver_doip::DoipDriver;
use opentune_udssim::{
    DoIpSimStream, DoIpUdpEntity, DtcRecord, EntityIdentity, TcpEntityServer, UdpEntityServer,
    UdsServer,
};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The simulated entity's logical address.
const ECU: u16 = 0x0010;

fn loopback() -> SocketAddr {
    (Ipv4Addr::LOCALHOST, 0).into()
}

/// The engine data the virtual car's demo pack expects, on one UDS server.
/// Placeholder identity only.
fn engine_ecu() -> UdsServer {
    let mut s = UdsServer::new();
    s.set_did(0xF187, "OTDOIP-0001");
    s.set_did(0xF188, "OTDOIP-SW-01");
    s.set_did(0xF190, "WVWZZZ00000000000");
    s.set_did(0xF197, "DOIPSIM");
    s.set_did(0x0600, vec![3, 3, 0, 0, 0, 0]); // coding/engine.json
    s.set_did(0x0610, vec![0x03, 0x20]); // idle target 800 rpm
    s.set_did(0x0611, vec![30]); // fan run-on 30 s
    s.set_routine(0x0203, vec![0x00]);
    s.set_io(0x0500, vec![0x00]);
    s.add_dtc(DtcRecord::new([0x01, 0x01, 0x00], 0x09));
    s.set_memory(
        0x0008_0000,
        (0..0x400u32).map(|i| (i * 7) as u8).collect::<Vec<u8>>(),
    );
    s
}

/// A DoIP entity on loopback: TCP data port (a fresh ECU per connection) and
/// UDP discovery port. Counts TCP connections.
struct Entity {
    tcp: TcpEntityServer,
    udp: UdpEntityServer,
    connections: Arc<AtomicUsize>,
}

fn entity() -> Entity {
    let connections = Arc::new(AtomicUsize::new(0));
    let n = connections.clone();
    let tcp = TcpEntityServer::spawn(loopback(), move || {
        n.fetch_add(1, Ordering::SeqCst);
        DoIpSimStream::new(engine_ecu(), ECU)
    })
    .unwrap();
    let udp =
        UdpEntityServer::spawn(loopback(), DoIpUdpEntity::new(EntityIdentity::new(ECU))).unwrap();
    Entity {
        tcp,
        udp,
        connections,
    }
}

impl Entity {
    fn tcp_port(&self) -> u16 {
        self.tcp.local_addr().port()
    }
    fn udp_port(&self) -> u16 {
        self.udp.local_addr().unwrap().port()
    }
    /// The address string a front end puts in `port`.
    fn spec(&self) -> String {
        format!("127.0.0.1:{}", self.tcp_port())
    }
}

fn cfg(pairs: &[(&str, String)]) -> ConnectConfig {
    let mut c = ConnectConfig::default();
    for (k, v) in pairs {
        c.options.insert((*k).into(), v.clone());
    }
    c
}

fn open(c: &ConnectConfig) -> DoipDriver {
    let mut d = DoipDriver::new();
    d.open(c).unwrap();
    d
}

fn pack(rel: &str) -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../packs/vcar-demo");
    std::fs::read_to_string(p.join(rel)).unwrap()
}

/// Run `f` over a freshly opened driver, like the CLI / desktop hub do.
fn with_diag<R>(c: &ConnectConfig, f: impl FnOnce(&mut CoreDiag) -> R) -> R {
    let mut d = open(c);
    let t = d.diag().expect("the doip driver must expose diag()");
    let mut diag = CoreDiag::new(t, 1000);
    f(&mut diag)
}

#[test]
fn open_by_address_identifies_and_learns_the_entity() {
    let e = entity();
    let mut d = open(&cfg(&[("port", e.spec())]));
    assert_eq!(d.target_address(), Some(ECU));
    assert_eq!(d.entity_address(), Some(ECU));
    assert!(!d.activation_confirmation_required());
    assert!(d.entity().is_none(), "no discovery for a direct address");
    let info = d.info();
    assert_eq!(info.id, "doip");
    assert!(info.firmware.unwrap().contains("0x0010"));
    let id = d.read_ecu_id().unwrap();
    assert_eq!(id.hw_number.as_deref(), Some("OTDOIP-0001"));
    assert_eq!(id.sw_number.as_deref(), Some("OTDOIP-SW-01"));
    assert_eq!(id.vin.as_deref(), Some("WVWZZZ00000000000"));
    assert_eq!(id.ecu_name.as_deref(), Some("DOIPSIM"));
    assert_eq!(id.raw["doip_target"], "0x0010");
    // Raw UDS through diag(): TesterPresent.
    let t = d.diag().unwrap();
    assert_eq!(t.request(&[0x3E, 0x00], 1000).unwrap(), vec![0x7E, 0x00]);
    d.close().unwrap();
    assert!(d.diag().is_none());
    assert_eq!(e.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn open_by_discovery_unicast_vin_and_eid() {
    let e = entity();
    let base = |extra: &str| {
        format!(
            "127.0.0.1?discover=1&udp_port={}&tcp_port={}{extra}",
            e.udp_port(),
            e.tcp_port()
        )
    };
    let d = open(&cfg(&[("port", base(""))]));
    let found = d.entity().expect("discovered");
    assert_eq!(found.info.logical_address, ECU);
    assert_eq!(found.info.vin_string(), "WVWZZZ00000000000");
    assert_eq!(d.target_address(), Some(ECU));
    assert_eq!(d.info().serial.as_deref(), Some("02:00:00:00:00:10"));
    drop(d);
    open(&cfg(&[("port", base("&vin=WVWZZZ00000000000"))]));
    open(&cfg(&[("port", base("&eid=020000000010"))]));
    // An EID nobody has: no answer, a clear error.
    let mut d = DoipDriver::new();
    let err = d
        .open(&cfg(&[
            ("port", base("&eid=090909090909")),
            ("discover_timeout_ms", "200".into()),
        ]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("no DoIP entity answered"), "{err}");
}

#[test]
fn explicit_target_and_tester() {
    let e = entity();
    // A target nobody serves: routing works, the diagnostic message is
    // refused by the entity (unknown target address) and reported.
    let mut d = open(&cfg(&[("port", format!("{}?target=0x0042", e.spec()))]));
    assert_eq!(d.target_address(), Some(0x0042));
    let err = d.diag().unwrap().request(&[0x3E, 0x00], 1000).unwrap_err();
    assert!(err.to_string().to_lowercase().contains("target"), "{err}");
    // The simulated entity serves one connection at a time.
    drop(d);
    // target=tx_id takes the logical address from the request id.
    let mut c = cfg(&[
        ("port", e.spec()),
        ("target", "tx_id".into()),
        ("tester", "0x0E80".into()),
    ]);
    c.tx_id = u32::from(ECU);
    let mut d = open(&c);
    assert_eq!(d.target_address(), Some(ECU));
    assert_eq!(
        d.diag().unwrap().request(&[0x3E, 0x00], 1000).unwrap(),
        vec![0x7E, 0x00]
    );
}

#[test]
fn nothing_listening_is_a_clear_connect_error() {
    // Bind then drop: the port is (very likely) closed now.
    let port = std::net::TcpListener::bind(loopback())
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut d = DoipDriver::new();
    let err = d
        .open(&cfg(&[
            ("port", format!("127.0.0.1:{port}")),
            ("timeout_ms", "500".into()),
        ]))
        .unwrap_err();
    assert!(matches!(err, Error::Io(_)), "{err}");
    assert!(err.to_string().contains("DoIP entity 127.0.0.1"), "{err}");
}

struct Xor(u8);
impl SeedKey for Xor {
    fn id(&self) -> &str {
        "test-xor"
    }
    fn compute(&self, seed: &[u8], _level: u8) -> opentune_core::Result<Vec<u8>> {
        Ok(seed.iter().map(|b| b ^ self.0).collect())
    }
}

#[test]
fn block_read_plan_and_security() {
    let e = entity();
    let region = MemoryRegion::new(0x0008_0100, 0x180);
    let want: Vec<u8> = (0x100..0x280u32).map(|i| (i * 7) as u8).collect();

    // Plain read (extended session by default), progress to the end.
    let mut d = open(&cfg(&[("port", e.spec())]));
    let mut last = None;
    let got = d.read_memory(region, &mut |p| last = Some(p)).unwrap();
    assert_eq!(got, want);
    assert_eq!(last.map(|p| (p.done, p.total)), Some((0x180, 0x180)));
    let plan = d.plan_read_memory(region).unwrap().unwrap();
    assert_eq!(
        plan,
        vec![
            "10 03",
            "23 44 00 08 01 00 00 00 01 00",
            "23 44 00 08 02 00 00 00 00 80",
        ]
    );
    // The simulated entity serves one connection at a time.
    drop(d);

    // security=required without a handler: refused before anything is sent.
    let mut d = open(&cfg(&[("port", format!("{}?security=required", e.spec()))]));
    let err = d.read_memory(region, &mut |_| {}).unwrap_err();
    assert!(matches!(err, Error::Safety(_)), "{err}");
    assert!(d.plan_read_memory(region).unwrap().is_err());
    drop(d);

    // With the simulator's test-only handler (seed XOR 0xA5) at level 0x01.
    let mut d = open(&cfg(&[(
        "port",
        format!(
            "{}?security=required&seedkey_level=01&read_chunk=0x80",
            e.spec()
        ),
    )]));
    assert!(d.set_seed_key(Box::new(Xor(0xA5))));
    assert_eq!(d.read_memory(region, &mut |_| {}).unwrap(), want);
    let plan = d.plan_read_memory(region).unwrap().unwrap();
    assert_eq!(&plan[..3], ["10 03", "27 01", "27 02 <key>"]);
    assert_eq!(plan.len(), 3 + 3);
    drop(d);

    // A wrong key is refused by the ECU and reported.
    let mut d = open(&cfg(&[("port", format!("{}?seedkey_level=01", e.spec()))]));
    d.set_seed_key(Box::new(Xor(0x00)));
    assert!(d.read_memory(region, &mut |_| {}).is_err());
    drop(d);

    // Writes are not offered: the module flasher runs on diag() instead.
    let mut d = open(&cfg(&[("port", e.spec())]));
    assert!(matches!(
        d.write_memory(region, &want, &mut |_| {}),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn coding_engine_reads_and_writes_over_doip() {
    use opentune_coding::{get, read_coding, set, write_coding, CodingDef, WriteOptions};
    let e = entity();
    let c = cfg(&[("port", e.spec())]);
    let def = CodingDef::from_json(&pack("coding/engine.json")).unwrap();
    def.validate().unwrap();
    with_diag(&c, |d| {
        let before = read_coding(d, &def).unwrap();
        assert_eq!(get(&def, &before, "Start-stop").unwrap(), 1);
        assert_eq!(get(&def, &before, "Fuel type").unwrap(), 1);
        assert_eq!(get(&def, &before, "Gearbox type").unwrap(), 1);
        // Within one connection (the simulator starts each connection fresh).
        let desired = set(&def, &before, "Speed limiter", 1).unwrap();
        let report = write_coding(d, &def, &before, &desired, &WriteOptions::default()).unwrap();
        assert!(report.is_verified(), "{:?}", report.outcome);
        let after = read_coding(d, &def).unwrap();
        assert_eq!(get(&def, &after, "Speed limiter").unwrap(), 1);
    });
}

#[test]
fn adaptation_engine_values_routine_and_actuator() {
    use opentune_adaptation::{
        read_value, run_actuator_test, run_routine, AdaptationDef, RoutineOp, WriteOptions,
    };
    let e = entity();
    let c = cfg(&[("port", e.spec())]);
    let def = AdaptationDef::from_json(&pack("adaptation/engine.json")).unwrap();
    let idle = with_diag(&c, |d| read_value(d, &def, "Idle speed target").unwrap());
    assert_eq!(idle.raw, 800);
    let fan = with_diag(&c, |d| read_value(d, &def, "Fan run-on time").unwrap());
    assert_eq!(fan.raw, 30);
    let r = with_diag(&c, |d| {
        run_routine(d, &def, "Throttle body adaptation", RoutineOp::Start, &[]).unwrap()
    });
    assert_eq!(r.value, Some(0.0));
    let a = with_diag(&c, |d| {
        run_actuator_test(d, &def, "Radiator fan", &[], &WriteOptions::default()).unwrap()
    });
    assert!(a.is_completed(), "{:?}", a.outcome);
}

#[test]
fn autoscan_engine_one_module_per_target() {
    use opentune_autoscan::{scan_module, ModuleAddr, Protocol};
    let e = entity();
    // A DoIP module map: req_id carries the logical address, `target=tx_id`.
    let module = |name: &str, addr: u16| ModuleAddr {
        name: name.into(),
        addr: addr as u8,
        req_id: u32::from(addr),
        resp_id: u32::from(addr),
        protocol: Protocol::Uds,
    };
    let scan = |m: &ModuleAddr| {
        let mut c = cfg(&[("port", format!("{}?target=tx_id", e.spec()))]);
        c.tx_id = m.req_id;
        c.rx_id = m.resp_id;
        with_diag(&c, |d| scan_module(d, m))
    };
    let engine = scan(&module("Engine", ECU));
    assert!(engine.present && engine.error.is_none(), "{engine:?}");
    let id = engine.identity.unwrap();
    assert_eq!(id.vin.as_deref(), Some("WVWZZZ00000000000"));
    assert_eq!(engine.dtcs.len(), 1);
    // Nobody at 0x0042: the entity refuses the target, the module is absent.
    let absent = scan(&module("Absent", 0x0042));
    assert!(!absent.present, "{absent:?}");
}

#[test]
fn service_function_runs_over_doip() {
    use opentune_servicefn::{run_script, RunOptions, ServiceScript};
    let e = entity();
    let c = cfg(&[("port", e.spec())]);
    let script = ServiceScript::from_json(&pack("services/engine-fan-test.json")).unwrap();
    let opts = RunOptions {
        dry_run: false,
        skip_waits: true,
    };
    let report = with_diag(&c, |d| run_script(d, &script, None, &opts).unwrap());
    assert!(report.succeeded(), "{:?}", report.outcome);
}

#[test]
fn live_logging_polls_dids_over_doip() {
    let e = entity();
    let c = cfg(&[("port", e.spec())]);
    let def = opentune_logdef::parse(
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
    let samples = with_diag(&c, |d| opentune_logdef::poll_once(d, &def, &[]));
    assert_eq!(samples.len(), 1, "{samples:?}");
    assert_eq!(samples[0].raw, Some(800), "{samples:?}");
}
