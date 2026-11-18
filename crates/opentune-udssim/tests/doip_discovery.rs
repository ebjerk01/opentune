//! DoIP vehicle discovery end to end: OpenTune's `DoIpUdpClient` against the
//! simulator's UDP entity on loopback (ephemeral ports, no broadcast), then
//! `connect_entity` over TCP into the simulated ECU. Offline only.

use opentune_core::protocol::doip::generic_nack;
use opentune_core::protocol::doip_discovery::{
    connect_entity, further_action, ConnectOptions, DoIpUdpClient, PowerMode, VehicleIdRequest,
};
use opentune_core::protocol::UdsClient;
use opentune_core::Error;
use opentune_udssim::doip_udp::{SIM_EID, SIM_VIN};
use opentune_udssim::{
    DoIpSimStream, DoIpUdpEntity, EntityIdentity, TcpEntityServer, UdpEntityServer, UdpFault,
    UdsServer,
};
use std::net::SocketAddr;
use std::time::Duration;

const ECU: u16 = 0x0010;
const LOOP: &str = "127.0.0.1:0";

fn udp_entity() -> UdpEntityServer {
    UdpEntityServer::spawn(
        LOOP.parse().unwrap(),
        DoIpUdpEntity::new(EntityIdentity::new(ECU)),
    )
    .unwrap()
}

fn client() -> DoIpUdpClient {
    DoIpUdpClient::bind(LOOP.parse().unwrap()).unwrap()
}

#[test]
fn identify_all_by_eid_and_by_vin() {
    let ent = udp_entity();
    let dest = ent.local_addr().unwrap();
    let c = client();

    let d = c.identify(dest, &VehicleIdRequest::All, 300).unwrap();
    assert_eq!(d.entities.len(), 1, "{d:?}");
    let e = &d.entities[0];
    assert_eq!(e.addr, dest, "answers come from the entity's own port");
    assert_eq!(e.info.logical_address, ECU);
    assert_eq!(&e.info.vin, SIM_VIN);
    assert_eq!(e.info.eid, SIM_EID);
    assert_eq!(e.info.sync_status, Some(0));
    assert_eq!((d.duplicates, d.rejected, d.ignored), (0, 0, 0));

    let by_eid = c
        .identify_first(dest, &VehicleIdRequest::ByEid(SIM_EID), 1000)
        .unwrap();
    assert_eq!(by_eid.entities.len(), 1);
    let other_eid = c
        .identify(dest, &VehicleIdRequest::ByEid([9; 6]), 150)
        .unwrap();
    assert!(other_eid.entities.is_empty(), "a non-matching EID is silent");

    let by_vin = c
        .identify_first(dest, &VehicleIdRequest::ByVin(*SIM_VIN), 1000)
        .unwrap();
    assert_eq!(by_vin.entities.len(), 1);
    let other_vin = c
        .identify(dest, &VehicleIdRequest::by_vin("XXXXXXXXXXXXXXXXX").unwrap(), 150)
        .unwrap();
    assert!(other_vin.entities.is_empty());
    assert_eq!(ent.entity().request_count(), 5);
}

#[test]
fn entity_status_and_power_mode() {
    let ent = udp_entity();
    let dest = ent.local_addr().unwrap();
    let c = client();
    let s = c.entity_status(dest, 1000).unwrap();
    assert_eq!(s, ent.entity().identity().status);
    assert_eq!(c.power_mode(dest, 1000).unwrap(), PowerMode::Ready);
    ent.entity().identity_mut().power_mode = PowerMode::NotReady;
    assert_eq!(c.power_mode(dest, 1000).unwrap(), PowerMode::NotReady);
}

#[test]
fn unsolicited_announcements_are_heard_once() {
    let ent = udp_entity();
    let listener = DoIpUdpClient::listener(LOOP.parse().unwrap()).unwrap();
    let to = listener.local_addr().unwrap();
    ent.announce(to, 3, Duration::from_millis(10)).unwrap();
    let d = listener.listen(300).unwrap();
    assert_eq!(d.entities.len(), 1, "{d:?}");
    assert_eq!(d.duplicates, 2);
    assert_eq!(d.entities[0].info.logical_address, ECU);
}

#[test]
fn fault_injection_no_reply_bad_version_nack() {
    let ent = udp_entity();
    let dest = ent.local_addr().unwrap();
    let c = client();

    ent.entity().inject_once(UdpFault::NoReply);
    assert!(matches!(c.power_mode(dest, 150), Err(Error::Timeout(150))));
    let d = c.identify(dest, &VehicleIdRequest::All, 150).unwrap();
    assert_eq!(d.entities.len(), 1, "one-shot fault is gone");

    ent.entity().inject_once(UdpFault::BadVersion);
    let d = c.identify(dest, &VehicleIdRequest::All, 150).unwrap();
    assert!(d.entities.is_empty());
    assert_eq!(d.rejected, 1, "a bad-version answer is dropped and counted");
    ent.entity().inject_once(UdpFault::BadVersion);
    match c.entity_status(dest, 150) {
        Err(Error::Protocol(m)) => assert!(m.contains("malformed"), "{m}"),
        other => panic!("expected a malformed-answer error, got {other:?}"),
    }

    ent.entity()
        .set_fault(Some(UdpFault::Nack(generic_nack::OUT_OF_MEMORY)));
    let d = c.identify(dest, &VehicleIdRequest::All, 150).unwrap();
    assert_eq!(d.nacks, vec![(dest, generic_nack::OUT_OF_MEMORY)]);
    match c.entity_status(dest, 1000) {
        Err(Error::Protocol(m)) => assert!(m.contains("outOfMemory"), "{m}"),
        other => panic!("expected a NACK error, got {other:?}"),
    }
}

#[test]
fn entity_nacks_a_bad_request_header() {
    let ent = udp_entity();
    let dest = ent.local_addr().unwrap();
    let mut c = client();
    // Version 0x09 is not a DoIP version: the entity answers NACK 0x00.
    c.set_protocol_version(0x09);
    let d = c.identify(dest, &VehicleIdRequest::All, 300).unwrap();
    assert_eq!(d.nacks, vec![(dest, generic_nack::INCORRECT_PATTERN)]);
    // The default version 0xFF is fine for identification requests.
    c.set_protocol_version(0xFF);
    let d = c.identify_first(dest, &VehicleIdRequest::All, 1000).unwrap();
    assert_eq!(d.entities.len(), 1);
}

fn seeded_server() -> UdsServer {
    let mut s = UdsServer::new();
    s.set_did(0xF190, SIM_VIN.to_vec());
    s
}

#[test]
fn discovered_entity_connects_in_one_call() {
    let ent = udp_entity();
    let tcp = TcpEntityServer::spawn(LOOP.parse().unwrap(), || {
        DoIpSimStream::new(seeded_server(), ECU)
    })
    .unwrap();
    let c = client();
    let d = c
        .identify_first(ent.local_addr().unwrap(), &VehicleIdRequest::All, 1000)
        .unwrap();
    let found = &d.entities[0];

    let opts = ConnectOptions {
        tcp_port: tcp.local_addr().port(),
        timeout_ms: 2000,
        ..ConnectOptions::default()
    };
    let mut tp = connect_entity(found, &opts).unwrap();
    assert!(tp.is_activated());
    assert_eq!(tp.entity_address(), Some(ECU));
    assert_eq!(tp.target_address, ECU, "target defaults to the announced address");
    let mut uds = UdsClient::new(&mut tp);
    assert_eq!(uds.read_data_by_id(0xF190).unwrap(), SIM_VIN.to_vec());
}

#[test]
fn central_security_further_action_selects_activation_type() {
    // The entity asks for central-security routing activation (0x10); the
    // simulator accepts type 0xE0, so the hop still succeeds.
    let mut id = EntityIdentity::new(ECU);
    id.info.further_action = further_action::CENTRAL_SECURITY;
    let ent = UdpEntityServer::spawn(LOOP.parse().unwrap(), DoIpUdpEntity::new(id)).unwrap();
    let tcp = TcpEntityServer::spawn(LOOP.parse().unwrap(), || {
        DoIpSimStream::new(seeded_server(), ECU)
    })
    .unwrap();
    let d = client()
        .identify_first(ent.local_addr().unwrap(), &VehicleIdRequest::All, 1000)
        .unwrap();
    assert_eq!(d.entities[0].info.further_action, 0x10);
    let opts = ConnectOptions {
        tcp_port: tcp.local_addr().port(),
        ..ConnectOptions::default()
    };
    assert!(connect_entity(&d.entities[0], &opts).unwrap().is_activated());
}

#[test]
fn ipv6_loopback_when_available() {
    let bind: SocketAddr = "[::1]:0".parse().unwrap();
    let Ok(ent) = UdpEntityServer::spawn(bind, DoIpUdpEntity::new(EntityIdentity::new(ECU)))
    else {
        eprintln!("skipping: no IPv6 loopback on this host");
        return;
    };
    let dest = ent.local_addr().unwrap();
    let c = DoIpUdpClient::for_destination(dest).unwrap();
    let d = c.identify_first(dest, &VehicleIdRequest::All, 1000).unwrap();
    assert_eq!(d.entities.len(), 1);
    assert!(d.entities[0].addr.is_ipv6());
    assert_eq!(c.power_mode(dest, 1000).unwrap(), PowerMode::Ready);
}
