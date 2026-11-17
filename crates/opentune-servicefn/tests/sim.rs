//! Integration test: run a (non-secured) service-function script against the
//! real `opentune-udssim` UDS conformance server, not a hand-rolled fake.

use opentune_core::CoreDiag;
use opentune_servicefn::{run_script, RunOptions, ServiceScript, Step};
use opentune_udssim::server::UdsServer;
use opentune_udssim::transport::SimTransport;

#[test]
fn runs_unsecured_script_against_the_conformance_server() {
    let mut server = UdsServer::new();
    server.set_did(0xF190, vec![0xABu8, 0xCD]);
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let script = ServiceScript {
        id: "svc".into(),
        name: "Read identity".into(),
        description: String::new(),
        requires_security: false,
        steps: vec![
            Step::DiagnosticSession { subfunction: 0x03 },
            Step::ReadDataByIdentifier { did: 0xF190 },
            Step::Verify {
                did: 0xF190,
                equals: vec![0xABu8, 0xCD],
            },
        ],
    };
    let opts = RunOptions {
        dry_run: false,
        skip_waits: true,
    };

    let report = run_script(&mut diag, &script, None, &opts).expect("run over the UDS sim");
    assert!(
        report.succeeded(),
        "script should complete, got {:?}",
        report.outcome
    );
}

#[test]
fn verify_mismatch_fails_the_script() {
    let mut server = UdsServer::new();
    server.set_did(0xF190, vec![0x00u8, 0x00]);
    let mut sim = SimTransport::new(server);
    let mut diag = CoreDiag::new(&mut sim, 1000);

    let script = ServiceScript {
        id: "svc".into(),
        name: "Verify".into(),
        description: String::new(),
        requires_security: false,
        steps: vec![Step::Verify {
            did: 0xF190,
            equals: vec![0xABu8, 0xCD], // does not match the seeded 00 00
        }],
    };
    let opts = RunOptions {
        dry_run: false,
        skip_waits: true,
    };

    let report = run_script(&mut diag, &script, None, &opts).expect("run completes");
    assert!(
        !report.succeeded(),
        "a Verify mismatch must not report success"
    );
}
