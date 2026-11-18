//! DoIP discovery for the sidebar connection box (Bench tier, alpha builds):
//! with the `doip` driver chosen, **Discover** sends one ISO 13400 vehicle
//! identification request and lists every DoIP entity that answers (VIN,
//! logical address, EID, IP). Picking one fills the port field with the
//! `doip` driver's address string, `IP?target=0xADDR`.
//!
//! Read-only on the network: one UDP request (broadcast, or unicast to the
//! address already in the port field), no TCP connection, nothing sent to an
//! ECU. Async with the wait on a blocking thread, never on the UI thread.

use crate::debug::alpha_only;
use crate::state::{BuildChannel, DebugLog};
use opentune_core::protocol::doip::DEFAULT_PORT;
use opentune_core::protocol::doip_discovery::{
    further_action, Discovery, DoIpUdpClient, VehicleIdRequest, A_DOIP_CTRL_MS, IPV4_BROADCAST,
};
use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};

/// Longest discovery wait the UI may ask for.
const MAX_TIMEOUT_MS: u64 = 10_000;

/// Where to send the identification request: the port field's host (UDP
/// `PORT` if given, else 13400), or the IPv4 broadcast when it is empty or
/// `discover`. Anything after `?` (the driver options) is ignored.
pub(crate) fn destination(spec: Option<&str>) -> Result<SocketAddr, String> {
    let host = spec.unwrap_or("").split('?').next().unwrap_or("").trim();
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
pub(crate) fn request(vin: Option<&str>, eid: Option<&str>) -> Result<VehicleIdRequest, String> {
    fn nz(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    match (nz(vin), nz(eid)) {
        (Some(_), Some(_)) => Err("give a VIN or an EID, not both".into()),
        (Some(v), None) => VehicleIdRequest::by_vin(v).map_err(|e| e.to_string()),
        (None, Some(e)) => VehicleIdRequest::by_eid(e).map_err(|e| e.to_string()),
        (None, None) => Ok(VehicleIdRequest::All),
    }
}

/// The UI shape of one discovery round.
pub(crate) fn discovery_json(dest: SocketAddr, d: &Discovery) -> Value {
    let entities: Vec<Value> = d
        .entities
        .iter()
        .map(|e| {
            let ip = e.addr.ip();
            // An IPv6 address needs brackets before `?`; keep a scope id.
            let host = match e.addr {
                SocketAddr::V6(v6) if v6.scope_id() != 0 => {
                    format!("[{}%{}]", v6.ip(), v6.scope_id())
                }
                SocketAddr::V6(v6) => format!("[{}]", v6.ip()),
                SocketAddr::V4(v4) => v4.ip().to_string(),
            };
            let addr = e.info.logical_address;
            json!({
                "ip": ip.to_string(),
                "udp_port": e.addr.port(),
                "vin": e.info.vin_string(),
                "logical_address": format!("0x{addr:04X}"),
                "eid": e.info.eid_string(),
                "gid": e.info.gid_string(),
                "further_action": further_action::name(e.info.further_action),
                "spec": format!("{host}?target=0x{addr:04X}"),
            })
        })
        .collect();
    let nacks: Vec<Value> = d
        .nacks
        .iter()
        .map(|(a, c)| json!({ "from": a.to_string(), "code": format!("0x{c:02X}") }))
        .collect();
    json!({
        "dest": dest.to_string(),
        "entities": entities,
        "nacks": nacks,
        "duplicates": d.duplicates,
        "rejected": d.rejected,
    })
}

/// One discovery round, blocking (the command runs it off the UI thread).
pub(crate) fn discover_blocking(
    dest: SocketAddr,
    req: &VehicleIdRequest,
    timeout_ms: u64,
) -> Result<Value, String> {
    let client = DoIpUdpClient::for_destination(dest).map_err(|e| e.to_string())?;
    let found = client
        .identify(dest, req, timeout_ms)
        .map_err(|e| format!("DoIP discovery to {dest}: {e}"))?;
    Ok(discovery_json(dest, &found))
}

/// Find DoIP entities: `{dest, entities: [{ip, vin, logical_address, eid,
/// gid, further_action, spec}], nacks, duplicates, rejected}`. `port` is the
/// connection box's port field (empty = IPv4 broadcast). Alpha builds only.
#[tauri::command]
pub async fn doip_discover(
    port: Option<String>,
    vin: Option<String>,
    eid: Option<String>,
    timeout_ms: Option<u64>,
    log: tauri::State<'_, DebugLog>,
    channel: tauri::State<'_, BuildChannel>,
) -> Result<Value, String> {
    alpha_only(&channel)?;
    let dest = destination(port.as_deref())?;
    let req = request(vin.as_deref(), eid.as_deref())?;
    let wait = timeout_ms
        .unwrap_or(A_DOIP_CTRL_MS)
        .clamp(1, MAX_TIMEOUT_MS);
    let res = tauri::async_runtime::spawn_blocking(move || discover_blocking(dest, &req, wait))
        .await
        .map_err(|e| e.to_string())?;
    match &res {
        Ok(v) => log.push(format!(
            "doip discover {dest}: {} entit{}",
            v["entities"].as_array().map_or(0, Vec::len),
            if v["entities"].as_array().map_or(0, Vec::len) == 1 {
                "y"
            } else {
                "ies"
            }
        )),
        Err(e) => log.push(format!("doip discover {dest}: {e}")),
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentune_udssim::{DoIpUdpEntity, EntityIdentity, UdpEntityServer};
    use std::net::Ipv4Addr;

    #[test]
    fn destinations() {
        let bc = SocketAddr::new(IPV4_BROADCAST.into(), 13400);
        assert_eq!(destination(None).unwrap(), bc);
        assert_eq!(destination(Some(" ")).unwrap(), bc);
        assert_eq!(destination(Some("discover?vin=X")).unwrap(), bc);
        assert_eq!(
            destination(Some("192.0.2.10?target=0x0010")).unwrap(),
            "192.0.2.10:13400".parse().unwrap()
        );
        assert_eq!(
            destination(Some("192.0.2.10:13401")).unwrap(),
            "192.0.2.10:13401".parse().unwrap()
        );
        assert_eq!(
            destination(Some("[fe80::10]")).unwrap(),
            "[fe80::10]:13400".parse().unwrap()
        );
        assert!(destination(Some("/dev/ttyUSB0"))
            .unwrap_err()
            .contains("not an IP"));
        assert_eq!(request(None, Some("")).unwrap(), VehicleIdRequest::All);
        assert!(request(Some("WVWZZZ00000000000"), Some("020000000010")).is_err());
        assert!(request(Some("SHORT"), None).is_err());
    }

    #[test]
    fn discovers_the_simulated_entity_and_builds_the_port_string() {
        let srv = UdpEntityServer::spawn(
            (Ipv4Addr::LOCALHOST, 0).into(),
            DoIpUdpEntity::new(EntityIdentity::new(0x0010)),
        )
        .unwrap();
        let dest = srv.local_addr().unwrap();
        let v = discover_blocking(dest, &VehicleIdRequest::All, 500).unwrap();
        let e = &v["entities"][0];
        assert_eq!(v["entities"].as_array().unwrap().len(), 1, "{v}");
        assert_eq!(e["ip"], "127.0.0.1");
        assert_eq!(e["vin"], "WVWZZZ00000000000");
        assert_eq!(e["logical_address"], "0x0010");
        assert_eq!(e["eid"], "02:00:00:00:00:10");
        assert_eq!(e["spec"], "127.0.0.1?target=0x0010");
        // A VIN nobody has: an empty list, not an error.
        let other = VehicleIdRequest::by_vin("WVWZZZ00000000001").unwrap();
        let v = discover_blocking(dest, &other, 200).unwrap();
        assert_eq!(v["entities"].as_array().unwrap().len(), 0, "{v}");
    }
}
