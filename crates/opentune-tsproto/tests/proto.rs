//! The protocol against the simulated ECU: framing, chunking, retries,
//! verified writes and burns, for envelope (Speeduino/rusEFI-style) and plain
//! (MegaSquirt-style) definitions.
use opentune_tsini::{decode_channels, load_ini, parse_ini, Ini, IniOptions, Tune, Value};
use opentune_tsproto::crc::crc32;
use opentune_tsproto::framing::{self, STATUS_CRC_FAILURE, STATUS_UNRECOGNIZED};
use opentune_tsproto::*;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

fn sample(name: &str) -> Ini {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../samples/ts")
        .join(name);
    load_ini(&p, &IniOptions::default()).unwrap()
}

fn opts() -> ClientOptions {
    ClientOptions {
        honor_delays: false,
        timeout: Some(Duration::from_millis(5)),
        ..ClientOptions::default()
    }
}

/// A tune with recognisable values in every page.
fn base_tune(ini: &Ini) -> Tune {
    let mut t = Tune::blank(ini);
    for (pi, p) in t.pages.iter_mut().enumerate() {
        for (i, b) in p.iter_mut().enumerate() {
            *b = (i * 3 + pi * 50) as u8;
        }
    }
    t
}

fn sim_with(ini: &Ini, t: &Tune) -> SimEcu {
    let mut s = SimEcu::new(ini, SimOptions::default());
    s.load_tune(t);
    s
}

/// Records what the client sends; optionally corrupts the first frame.
struct Tap<P: Port> {
    inner: P,
    sent: Vec<Vec<u8>>,
    corrupt_first: bool,
}

impl<P: Port> Port for Tap<P> {
    fn send(&mut self, data: &[u8]) -> io::Result<()> {
        self.sent.push(data.to_vec());
        if self.corrupt_first {
            self.corrupt_first = false;
            let mut d = data.to_vec();
            let last = d.len() - 1;
            d[last] ^= 0x80;
            return self.inner.send(&d);
        }
        self.inner.send(data)
    }
    fn recv(&mut self, buf: &mut [u8], t: Duration) -> io::Result<usize> {
        self.inner.recv(buf, t)
    }
    fn discard_input(&mut self) -> io::Result<()> {
        self.inner.discard_input()
    }
}

#[test]
fn envelope_bytes_on_the_wire() {
    let ini = sample("opentune-demo.ini");
    let tap = Tap {
        inner: sim_with(&ini, &base_tune(&ini)),
        sent: Vec::new(),
        corrupt_first: false,
    };
    let mut c = Client::new(tap, &ini, opts());
    assert_eq!(c.framing(), Framing::Envelope);
    assert_eq!(c.query_signature().unwrap(), "opentune demo 1");
    let page = c.read_page(0, &mut |_, _| {}).unwrap();
    assert_eq!(page, base_tune(&ini).pages[0]);
    let tap = c.into_port();
    assert_eq!(tap.sent[0], framing::wrap(b"Q").unwrap());
    // p, page id ($tsCanId=0, 1), offset 0 LE, count 32 LE (blockingFactor).
    assert_eq!(
        tap.sent[1],
        framing::wrap(&[b'p', 0, 1, 0, 0, 32, 0]).unwrap()
    );
    assert_eq!(
        tap.sent[2],
        framing::wrap(&[b'p', 0, 1, 32, 0, 32, 0]).unwrap()
    );
    assert_eq!(tap.sent.len(), 3, "64-byte page in two 32-byte chunks");
    let w = &tap.sent[1];
    assert_eq!(&w[..2], &[0, 7]);
    assert_eq!(&w[9..], &crc32(&w[2..9]).to_be_bytes());
}

#[test]
fn signature_check() {
    let ini = sample("opentune-demo.ini");
    let mut s = SimEcu::new(
        &ini,
        SimOptions {
            signature: Some("speeduino 202504".into()),
            ..SimOptions::default()
        },
    );
    s.load_tune(&Tune::blank(&ini));
    let mut c = Client::new(s, &ini, opts());
    let e = c.check_signature(false).unwrap_err();
    assert!(matches!(e, ProtoError::SignatureMismatch { .. }), "{e}");
    let ok = c.check_signature(true).unwrap();
    assert!(!ok.matches);
    assert_eq!(ok.ecu, "speeduino 202504");
}

#[test]
fn reads_whole_tune_in_chunks() {
    let ini = sample("opentune-demo.ini");
    let base = base_tune(&ini);
    let mut c = Client::new(sim_with(&ini, &base), &ini, opts());
    let mut calls = 0;
    let t = c.read_tune(&mut |_, _, _| calls += 1).unwrap();
    assert_eq!(t, base);
    assert_eq!(calls, 2 + 4, "64/32 + 112/32 chunks");
    let reads: Vec<_> = c
        .port_mut()
        .events()
        .iter()
        .filter_map(|e| match e {
            SimEvent::Read { page, offset, len } => Some((*page, *offset, *len)),
            _ => None,
        })
        .collect();
    assert_eq!(reads.last(), Some(&(1, 96, 16)));
    assert_eq!(c.page_crc32(1).unwrap(), Some(crc32(&base.pages[1])));
}

#[test]
fn realtime_block_decodes() {
    let ini = sample("opentune-demo.ini");
    let mut s = sim_with(&ini, &Tune::blank(&ini));
    let mut block = vec![0u8; 12];
    block[4] = 130; // coolantRaw
    block[5..7].copy_from_slice(&2500u16.to_le_bytes());
    s.set_realtime(block.clone());
    let mut c = Client::new(s, &ini, opts());
    let b = c.read_realtime().unwrap();
    assert_eq!(b, block);
    let samples = decode_channels(&ini, &b, &|_| None, Some(&["rpm".into(), "coolant".into()]));
    assert_eq!(samples[0].value, Some(2500.0));
    assert_eq!(samples[1].value, Some(90.0));
    assert!(matches!(
        c.port_mut().events().last(),
        Some(SimEvent::Realtime { offset: 0, len: 12 })
    ));
}

#[test]
fn transient_faults_are_retried() {
    let ini = sample("opentune-demo.ini");
    let base = base_tune(&ini);
    for f in [
        Fault::CorruptResponse,
        Fault::TruncateResponse(5),
        Fault::DropResponse,
        Fault::Status(STATUS_CRC_FAILURE),
        Fault::Status(0x85),
    ] {
        let mut s = sim_with(&ini, &base);
        s.inject(f.clone());
        let mut c = Client::new(s, &ini, opts());
        let p = c.read_page(1, &mut |_, _| {}).unwrap();
        assert_eq!(p, base.pages[1], "{f:?}");
        assert_eq!(c.stats().retries, 1, "{f:?}");
    }
}

#[test]
fn corrupted_request_is_rejected_and_resent() {
    let ini = sample("opentune-demo.ini");
    let tap = Tap {
        inner: sim_with(&ini, &Tune::blank(&ini)),
        sent: Vec::new(),
        corrupt_first: true,
    };
    let mut c = Client::new(tap, &ini, opts());
    assert_eq!(c.query_signature().unwrap(), "opentune demo 1");
    assert_eq!(c.stats().retries, 1);
    assert_eq!(c.port_mut().inner.events()[0], SimEvent::BadRequestCrc);
}

#[test]
fn hard_errors_and_exhausted_retries_fail() {
    let ini = sample("opentune-demo.ini");
    let mut s = sim_with(&ini, &Tune::blank(&ini));
    s.inject(Fault::Status(STATUS_UNRECOGNIZED));
    let mut c = Client::new(s, &ini, opts());
    let e = c.read_page(0, &mut |_, _| {}).unwrap_err();
    assert!(matches!(e, ProtoError::Ecu { code: 0x83, .. }), "{e}");
    assert_eq!(c.stats().retries, 0, "not retried");

    let mut s = sim_with(&ini, &Tune::blank(&ini));
    for _ in 0..3 {
        s.inject(Fault::CorruptResponse);
    }
    let mut c = Client::new(s, &ini, opts());
    let e = c.query_signature().unwrap_err();
    assert!(matches!(e, ProtoError::Crc { .. }), "{e}");
    assert_eq!(c.stats().retries, 2);
}

fn edited(ini: &Ini, base: &Tune) -> Tune {
    let mut t = base.clone();
    t.set(ini, "reqFuel", &Value::Scalar(9.9)).unwrap();
    t.set(
        ini,
        "veTable",
        &Value::Array((0..48).map(|i| 40.0 + i as f64).collect()),
    )
    .unwrap();
    t
}

#[test]
fn verified_write_then_burn() {
    let ini = sample("opentune-demo.ini");
    let base = base_tune(&ini);
    let target = edited(&ini, &base);
    let plan = plan_write(&base, &target);
    assert_eq!(plan.pages.len(), 2);
    assert_eq!(plan.pages[0].ranges, vec![(0, 1)]);
    let mut c = Client::new(sim_with(&ini, &base), &ini, opts());
    let auth = WriteAuth::user_confirmed();
    let rep = write_tune(
        &mut c,
        &auth,
        &base,
        &target,
        WriteOptions::default(),
        &mut |_, _, _, _| {},
    )
    .unwrap();
    assert_eq!(rep.pages, vec![0, 1]);
    assert_eq!(rep.burned, vec![0, 1]);
    assert_eq!(rep.crc_checked, vec![0, 1]);
    let sim = c.into_port();
    assert_eq!(sim.flash(), target.pages.as_slice());
    // Only changed bytes were sent, each request within blockingFactor.
    let writes: Vec<(usize, usize, usize)> = sim
        .events()
        .iter()
        .filter_map(|e| match e {
            SimEvent::Write { page, offset, len } => Some((*page, *offset, *len)),
            _ => None,
        })
        .collect();
    assert_eq!(writes[0], (0, 0, 1));
    assert!(writes.iter().all(|w| w.2 <= 32 - 7), "{writes:?}");
    assert_eq!(
        writes
            .iter()
            .filter(|w| w.0 == 1)
            .map(|w| w.2)
            .sum::<usize>(),
        48
    );
    // Burns come after every verify read.
    let first_burn = sim
        .events()
        .iter()
        .position(|e| matches!(e, SimEvent::Burn { .. }))
        .unwrap();
    let last_read = sim
        .events()
        .iter()
        .rposition(|e| matches!(e, SimEvent::Read { .. }))
        .unwrap();
    assert!(last_read < first_burn);
}

#[test]
fn without_burn_the_change_is_ram_only() {
    let ini = sample("opentune-demo.ini");
    let base = base_tune(&ini);
    let target = edited(&ini, &base);
    let mut c = Client::new(sim_with(&ini, &base), &ini, opts());
    let rep = write_tune(
        &mut c,
        &WriteAuth::user_confirmed(),
        &base,
        &target,
        WriteOptions {
            burn: false,
            check_crc: false,
        },
        &mut |_, _, _, _| {},
    )
    .unwrap();
    assert!(rep.burned.is_empty());
    let mut sim = c.into_port();
    assert_eq!(sim.ram(), target.pages.as_slice());
    assert_eq!(sim.flash(), base.pages.as_slice());
    sim.power_cycle();
    assert_eq!(sim.ram(), base.pages.as_slice());
}

#[test]
fn verify_failure_burns_nothing() {
    let ini = sample("opentune-demo.ini");
    let base = base_tune(&ini);
    let target = edited(&ini, &base);
    for f in [Fault::IgnoreWrite, Fault::CorruptWrite] {
        let mut s = sim_with(&ini, &base);
        s.inject(f.clone());
        let mut c = Client::new(s, &ini, opts());
        let e = write_tune(
            &mut c,
            &WriteAuth::user_confirmed(),
            &base,
            &target,
            WriteOptions::default(),
            &mut |_, _, _, _| {},
        )
        .unwrap_err();
        assert!(
            matches!(
                e,
                ProtoError::Verify {
                    page: 1,
                    offset: 0,
                    ..
                }
            ),
            "{f:?}: {e}"
        );
        let sim = c.into_port();
        assert!(!sim
            .events()
            .iter()
            .any(|e| matches!(e, SimEvent::Burn { .. })));
        assert_eq!(sim.flash(), base.pages.as_slice(), "{f:?}");
    }
}

#[test]
fn plain_megasquirt_style() {
    let ini = sample("opentune-demo-ms.ini");
    let mut base = Tune::blank(&ini);
    base.set(
        &ini,
        "rpmBins",
        &Value::Array(vec![800.0, 2000.0, 4000.0, 6500.0]),
    )
    .unwrap();
    let tap = Tap {
        inner: sim_with(&ini, &base),
        sent: Vec::new(),
        corrupt_first: false,
    };
    let mut c = Client::new(tap, &ini, opts());
    assert_eq!(c.framing(), Framing::Plain);
    assert_eq!(c.query_signature().unwrap(), "OT-MSDEMO 1.0");
    let t = c.read_tune(&mut |_, _, _| {}).unwrap();
    assert_eq!(t, base);
    assert_eq!(c.read_realtime().unwrap().len(), 8);
    let mut target = base.clone();
    target.set(&ini, "reqFuel", &Value::Scalar(10.5)).unwrap();
    target.set(&ini, "mode", &Value::Bits(2)).unwrap();
    let rep = write_tune(
        &mut c,
        &WriteAuth::user_confirmed(),
        &base,
        &target,
        WriteOptions::default(),
        &mut |_, _, _, _| {},
    )
    .unwrap();
    assert_eq!(rep.burned, vec![0]);
    assert!(
        rep.crc_checked.is_empty(),
        "no crc32CheckCommand in this INI"
    );
    let tap = c.into_port();
    // Raw bytes: r, canId, page 4, offset BE, count BE.
    assert_eq!(tap.sent[1], vec![b'r', 0, 4, 0, 0, 0, 16]);
    assert_eq!(tap.sent[0], b"Q".to_vec());
    assert!(tap.sent.iter().any(|r| r == &vec![b'b', 0, 4]));
    let w = tap.sent.iter().find(|r| r[0] == b'w').unwrap();
    // w, canId, page, offset 28 BE, count 5 BE (28..33 merged), values.
    assert_eq!(&w[..7], &[b'w', 0, 4, 0, 28, 0, 5]);
    assert_eq!(tap.inner.flash(), target.pages.as_slice());
}

#[test]
fn plain_short_reply_times_out_and_retries() {
    let ini = sample("opentune-demo-ms.ini");
    let mut s = sim_with(&ini, &Tune::blank(&ini));
    s.inject(Fault::TruncateResponse(3));
    let mut c = Client::new(s, &ini, opts());
    c.read_page(0, &mut |_, _| {}).unwrap();
    assert_eq!(c.stats().retries, 1);
    let mut s = sim_with(&ini, &Tune::blank(&ini));
    for _ in 0..3 {
        s.inject(Fault::TruncateResponse(3));
    }
    let mut c = Client::new(s, &ini, opts());
    let e = c.read_page(0, &mut |_, _| {}).unwrap_err();
    assert!(
        matches!(
            e,
            ProtoError::Timeout {
                got: 3,
                expected: 16,
                ..
            }
        ),
        "{e}"
    );
}

const RUSEFI_LIKE: &str = r#"
[TunerStudio]
   queryCommand = "S"
   signature = "rusEFI test.2026"
[Constants]
   messageEnvelopeFormat = msEnvelope_1.0
   endianness = little
   nPages = 2
   pageIdentifier = "\x00\x00", "\x00\x01"
   pageSize = 40, 20
   pageReadCommand = "R%2i%2o%2c", "R%2i%2o%2c"
   burnCommand = "B%2i", ""
   pageChunkWrite = "C%2i%2o%2c%v", "C%2i%2o%2c%v"
   crc32CheckCommand = "k%2i%2o%2c", "k%2i%2o%2c"
   blockingFactor = 1024
page = 1
   a = scalar, F32, 0, "", 1, 0, -1e6, 1e6, 3
   b = array, S16, 4, [2x3], "", 0.01, 0, -300, 300, 2
page = 2
   scatter = array, U16, 0, [10], "", 1, 0, 0, 65535, 0
[OutputChannels]
   ochGetCommand = "O%2o%2c"
   ochBlockSize = 2100
   rpm = scalar, U16, 2000, "rpm", 1, 0
"#;

#[test]
fn rusefi_style_definition() {
    let ini = parse_ini(RUSEFI_LIKE, &IniOptions::default());
    assert!(ini.is_clean(), "{:?}", ini.warnings);
    let mut s = SimEcu::new(
        &ini,
        SimOptions {
            signature_nul: true,
            ..SimOptions::default()
        },
    );
    let mut block = vec![0u8; 2100];
    block[2000..2002].copy_from_slice(&6500u16.to_le_bytes());
    s.set_realtime(block);
    let tap = Tap {
        inner: s,
        sent: Vec::new(),
        corrupt_first: false,
    };
    let mut c = Client::new(tap, &ini, opts());
    let sig = c.check_signature(false).unwrap();
    assert!(sig.matches, "trailing NUL ignored");
    let rt = c.read_realtime().unwrap();
    assert_eq!(rt.len(), 2100);
    let v = decode_channels(&ini, &rt, &|_| None, Some(&["rpm".into()]));
    assert_eq!(v[0].value, Some(6500.0));
    let base = c.read_tune(&mut |_, _, _| {}).unwrap();
    let mut target = base.clone();
    target.set(&ini, "a", &Value::Scalar(-2.5)).unwrap();
    target
        .set(
            &ini,
            "b",
            &Value::Array(vec![1.0, -2.0, 3.5, 0.0, 0.0, -0.01]),
        )
        .unwrap();
    target
        .set(
            &ini,
            "scatter",
            &Value::Array((0..10).map(|i| i as f64 * 1000.0).collect()),
        )
        .unwrap();
    let rep = write_tune(
        &mut c,
        &WriteAuth::user_confirmed(),
        &base,
        &target,
        WriteOptions::default(),
        &mut |_, _, _, _| {},
    )
    .unwrap();
    assert_eq!(rep.burned, vec![0]);
    assert_eq!(rep.not_burnable, vec![1], "page 2 has an empty burnCommand");
    let tap = c.into_port();
    assert_eq!(tap.sent[0], framing::wrap(b"S").unwrap());
    // R, page 0x0001 as written in the INI, offset 0, count 20 (LE).
    assert!(tap
        .sent
        .contains(&framing::wrap(&[b'R', 0, 1, 0, 0, 20, 0]).unwrap()));
    assert_eq!(&tap.inner.ram()[0][..4], &(-2.5f32).to_le_bytes());
    assert_eq!(&tap.inner.ram()[0][4..6], &100i16.to_le_bytes());
}
