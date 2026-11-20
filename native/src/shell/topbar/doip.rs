//! DoIP discovery for Adapter settings (row 26; Bench tier, alpha builds),
//! the native side of the desktop's `feat_doip.rs`: with the `doip` driver
//! chosen, **Discover** sends one ISO 13400 vehicle identification request
//! and lists every DoIP entity that answers (VIN, logical address, EID, IP).
//! Picking one fills the port with the `doip` driver's address string,
//! `IP?target=0xADDR`.
//!
//! Read-only on the network: one UDP request (broadcast, or unicast to the
//! address already in the port field), no TCP connection, nothing sent to an
//! ECU. [`discover`] is gated on the build channel itself (refused on
//! stable before a socket is opened), not only hidden in the UI; it blocks,
//! so the top bar runs it on a background thread.

use crate::host::{bench_channel, BuildChannel};
use opentune_core::protocol::doip::DEFAULT_PORT;
use opentune_core::protocol::doip_discovery::{
    further_action, Discovery, DoIpUdpClient, VehicleIdRequest, A_DOIP_CTRL_MS, IPV4_BROADCAST,
};
use std::net::{IpAddr, SocketAddr};

/// Longest discovery wait.
const MAX_TIMEOUT_MS: u64 = 10_000;

/// One entity that answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entity {
    pub ip: String,
    pub vin: String,
    pub logical_address: u16,
    pub eid: String,
    pub further_action: &'static str,
    /// The `doip` driver's port string for this entity.
    pub spec: String,
}

/// One discovery round.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Found {
    pub dest: String,
    pub entities: Vec<Entity>,
    pub nacks: usize,
    pub rejected: usize,
}

/// `Err` unless the channel enables the Bench tier.
pub fn gate(channel: &BuildChannel) -> Result<(), String> {
    if bench_channel(channel) {
        Ok(())
    } else {
        Err(format!(
            "DoIP discovery is a Bench-tier tool: refused on the '{}' channel (alpha builds only)",
            channel.as_str()
        ))
    }
}

/// Where to send the request: the port field's host (UDP `PORT` if given,
/// else 13400), or the IPv4 broadcast when it is empty or `discover`.
/// Anything after `?` (the driver options) is ignored.
pub fn destination(spec: &str) -> Result<SocketAddr, String> {
    let host = spec.split('?').next().unwrap_or("").trim();
    if host.is_empty() || host.eq_ignore_ascii_case("discover") {
        return Ok(SocketAddr::new(IPV4_BROADCAST.into(), DEFAULT_PORT));
    }
    if let Ok(sa) = host.parse::<SocketAddr>() {
        return Ok(sa);
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<IpAddr>()
        .map(|ip| SocketAddr::new(ip, DEFAULT_PORT))
        .map_err(|_| format!("'{host}' is not an IP address (leave it empty to broadcast)"))
}

/// The identification request: all entities, or by VIN / EID.
pub fn request(vin: &str, eid: &str) -> Result<VehicleIdRequest, String> {
    match (vin.trim(), eid.trim()) {
        ("", "") => Ok(VehicleIdRequest::All),
        (v, "") => VehicleIdRequest::by_vin(v).map_err(|e| e.to_string()),
        ("", e) => VehicleIdRequest::by_eid(e).map_err(|e| e.to_string()),
        _ => Err("give a VIN or an EID, not both".into()),
    }
}

fn found(dest: SocketAddr, d: &Discovery) -> Found {
    let entities = d
        .entities
        .iter()
        .map(|e| {
            // An IPv6 address needs brackets before `?`; keep a scope id.
            let host = match e.addr {
                SocketAddr::V6(v6) if v6.scope_id() != 0 => {
                    format!("[{}%{}]", v6.ip(), v6.scope_id())
                }
                SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
                SocketAddr::V4(v4) => v4.ip().to_string(),
            };
            let addr = e.info.logical_address;
            Entity {
                ip: e.addr.ip().to_string(),
                vin: e.info.vin_string(),
                logical_address: addr,
                eid: e.info.eid_string(),
                further_action: further_action::name(e.info.further_action),
                spec: format!("{host}?target=0x{addr:04X}"),
            }
        })
        .collect();
    Found {
        dest: dest.to_string(),
        entities,
        nacks: d.nacks.len(),
        rejected: d.rejected,
    }
}

/// One discovery round (blocking). Refused unless `channel` enables the
/// Bench tier; `port` is the Adapter settings port field.
pub fn discover(
    channel: &BuildChannel,
    port: &str,
    vin: &str,
    eid: &str,
    timeout_ms: Option<u64>,
) -> Result<Found, String> {
    gate(channel)?;
    let dest = destination(port)?;
    let req = request(vin, eid)?;
    let wait = timeout_ms
        .unwrap_or(A_DOIP_CTRL_MS)
        .clamp(1, MAX_TIMEOUT_MS);
    let client = DoIpUdpClient::for_destination(dest).map_err(|e| e.to_string())?;
    let d = client
        .identify(dest, &req, wait)
        .map_err(|e| format!("DoIP discovery to {dest}: {e}"))?;
    let f = found(dest, &d);
    crate::views::debug::session::push(format!(
        "doip discover {dest}: {} entit{}",
        f.entities.len(),
        if f.entities.len() == 1 { "y" } else { "ies" }
    ));
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_udssim::{DoIpUdpEntity, EntityIdentity, UdpEntityServer};
    use std::net::Ipv4Addr;

    #[test]
    fn destinations_and_requests() {
        let bc = SocketAddr::new(IPV4_BROADCAST.into(), 13400);
        assert_eq!(destination("").unwrap(), bc);
        assert_eq!(destination("discover?vin=X").unwrap(), bc);
        assert_eq!(
            destination("192.0.2.10?target=0x0010").unwrap(),
            "192.0.2.10:13400".parse().unwrap()
        );
        assert_eq!(
            destination("192.0.2.10:13401").unwrap(),
            "192.0.2.10:13401".parse().unwrap()
        );
        assert_eq!(
            destination("[fe80::10]").unwrap(),
            "[fe80::10]:13400".parse().unwrap()
        );
        assert!(destination("/dev/ttyUSB0")
            .unwrap_err()
            .contains("not an IP"));
        assert_eq!(request("", " ").unwrap(), VehicleIdRequest::All);
        assert!(request("WVWZZZ00000000000", "020000000010").is_err());
        assert!(request("SHORT", "").is_err());
    }

    #[test]
    fn stable_channel_is_refused_before_any_socket() {
        let stable = BuildChannel::new(BuildChannel::STABLE);
        // Even a malformed port is not looked at: the gate comes first.
        let e = discover(&stable, "not-an-ip", "", "", Some(10)).unwrap_err();
        assert!(e.contains("Bench-tier") && e.contains("stable"), "{e}");
        assert!(gate(&BuildChannel::new(BuildChannel::BENCH)).is_ok());
    }

    #[test]
    fn discovers_the_simulated_entity_on_alpha() {
        let srv = UdpEntityServer::spawn(
            (Ipv4Addr::LOCALHOST, 0).into(),
            DoIpUdpEntity::new(EntityIdentity::new(0x0010)),
        )
        .unwrap();
        let dest = srv.local_addr().unwrap().to_string();
        let alpha = BuildChannel::new(BuildChannel::ALPHA);
        let f = discover(&alpha, &dest, "", "", Some(500)).unwrap();
        assert_eq!(f.entities.len(), 1, "{f:?}");
        let e = &f.entities[0];
        assert_eq!(e.ip, "127.0.0.1");
        assert_eq!(e.vin, "WVWZZZ00000000000");
        assert_eq!(e.logical_address, 0x0010);
        assert_eq!(e.eid, "02:00:00:00:00:10");
        assert_eq!(e.spec, "127.0.0.1?target=0x0010");
        // A VIN nobody has: an empty list, not an error.
        let f = discover(&alpha, &dest, "WVWZZZ00000000001", "", Some(200)).unwrap();
        assert!(f.entities.is_empty());
    }
}
