//! `opentune doip`: ISO 13400 DoIP over Ethernet — discover entities (vehicle
//! identification request or unsolicited announcements), read an entity's
//! status and diagnostic power mode over UDP, and optionally check routing
//! activation on what was found. Read-only: nothing is sent to an ECU beyond
//! the DoIP control messages and the routing-activation handshake.
//!
//! `--sim` runs the same code against the `opentune-udssim` DoIP entity
//! (UDP + TCP) on loopback.
use crate::util::parse_num;
use anyhow::{anyhow, bail, Context, Result};
use opentune_core::protocol::doip::{generic_nack, DEFAULT_PORT};
use opentune_core::protocol::doip_discovery::{
    connect_entity, further_action, node_type, sync_status, ConnectOptions, DiscoveredEntity,
    Discovery, DoIpUdpClient, VehicleIdRequest, IPV4_BROADCAST, IPV6_ALL_NODES,
};
use opentune_udssim::doip_udp::SIM_VIN;
use opentune_udssim::{
    DoIpSimStream, DoIpUdpEntity, EntityIdentity, TcpEntityServer, UdpEntityServer, UdpFault,
    UdsServer,
};
use serde_json::{json, Value};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

#[derive(clap::Subcommand)]
pub enum DoipAction {
    /// Find DoIP entities: send a vehicle identification request (every
    /// entity, or `--eid` / `--vin`) and list each answer once, or `--listen`
    /// for unsolicited vehicle announcements. `--activate` then checks routing
    /// activation on each entity over TCP.
    Discover(DiscoverArgs),
    /// An entity's status over UDP: node type, open / max TCP sockets, max
    /// data size.
    Status(NetArgs),
    /// An entity's diagnostic power mode over UDP (ready / not ready).
    Power(NetArgs),
}

#[derive(clap::Args)]
pub struct NetArgs {
    /// Entity IP. `discover` defaults to the IPv4 broadcast 255.255.255.255
    /// (`ff02::1` with `--ipv6`); `status` / `power` need one.
    #[arg(long)]
    ip: Option<IpAddr>,
    /// UDP discovery port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    port: u16,
    /// IPv6 scope (interface index) for a link-local or multicast address.
    #[arg(long, value_name = "INDEX")]
    scope_id: Option<u32>,
    /// Header protocol version: 0x02 (ISO 13400-2:2012), 0x03 (2019), or 0xFF
    /// (the default version, vehicle identification requests only).
    #[arg(long, default_value = "0x02", value_name = "VERSION")]
    protocol_version: String,
    /// How long to wait for answers.
    #[arg(long, default_value_t = 2000)]
    timeout_ms: u64,
    /// Use the built-in simulated DoIP entity on loopback instead of a network.
    #[arg(long, conflicts_with = "ip")]
    sim: bool,
    /// Make the simulated entity misbehave: no-reply, bad-version, nack:CODE.
    #[arg(long, requires = "sim", value_name = "FAULT")]
    sim_fault: Option<String>,
    /// JSON output.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct DiscoverArgs {
    #[command(flatten)]
    net: NetArgs,
    /// Only the entity with this EID answers (6 hex bytes, `:` optional).
    #[arg(long, conflicts_with_all = ["vin", "listen"])]
    eid: Option<String>,
    /// Only entities of the vehicle with this VIN answer.
    #[arg(long, conflicts_with = "listen")]
    vin: Option<String>,
    /// Broadcast over IPv6 (`ff02::1`) instead of IPv4.
    #[arg(long)]
    ipv6: bool,
    /// Send nothing; collect unsolicited vehicle announcements instead.
    #[arg(long)]
    listen: bool,
    /// Local address to listen on with `--listen` (default 0.0.0.0:13400, or
    /// [::]:13400 with `--ipv6`).
    #[arg(long, requires = "listen", value_name = "ADDR")]
    bind: Option<SocketAddr>,
    /// Connect each entity's TCP data port and run routing activation.
    #[arg(long)]
    activate: bool,
    /// Tester logical address for `--activate`.
    #[arg(long, default_value = "0x0E00", value_name = "ADDR")]
    tester: String,
    /// TCP data port for `--activate`.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    tcp_port: u16,
}

pub fn run(action: DoipAction) -> Result<()> {
    match action {
        DoipAction::Discover(a) => discover(a),
        DoipAction::Status(a) => status(a),
        DoipAction::Power(a) => power(a),
    }
}

/// The simulated entity (UDP + TCP sides) on loopback.
struct Sim {
    udp: UdpEntityServer,
    tcp: TcpEntityServer,
}

const SIM_ECU: u16 = 0x0010;

fn start_sim(fault: Option<&str>, ipv6: bool) -> Result<Sim> {
    let ip: IpAddr = if ipv6 {
        Ipv6Addr::LOCALHOST.into()
    } else {
        Ipv4Addr::LOCALHOST.into()
    };
    let mut entity = DoIpUdpEntity::new(EntityIdentity::new(SIM_ECU));
    if let Some(f) = fault {
        entity.set_fault(Some(f.parse::<UdpFault>().map_err(|e| anyhow!(e))?));
    }
    let udp = UdpEntityServer::spawn((ip, 0).into(), entity)
        .context("start the simulated DoIP entity (UDP)")?;
    let tcp = TcpEntityServer::spawn((ip, 0).into(), || {
        let mut ecu = UdsServer::new();
        ecu.set_did(0xF190, SIM_VIN.to_vec());
        DoIpSimStream::new(ecu, SIM_ECU)
    })
    .context("start the simulated DoIP entity (TCP)")?;
    Ok(Sim { udp, tcp })
}

fn version(net: &NetArgs) -> Result<u8> {
    u8::try_from(parse_num(&net.protocol_version)?)
        .map_err(|_| anyhow!("--protocol-version is one byte"))
}

fn dest_addr(ip: IpAddr, port: u16, scope: Option<u32>) -> SocketAddr {
    match (ip, scope) {
        (IpAddr::V6(v6), Some(s)) => SocketAddrV6::new(v6, port, 0, s).into(),
        _ => SocketAddr::new(ip, port),
    }
}

fn is_group(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.octets()[3] == 255,
        IpAddr::V6(v6) => v6.is_multicast(),
    }
}

fn hex16(v: u16) -> String {
    format!("0x{v:04X}")
}

fn entity_json(e: &DiscoveredEntity) -> Value {
    let i = &e.info;
    json!({
        "addr": e.addr.to_string(),
        "logical_address": hex16(i.logical_address),
        "vin": i.vin_string(),
        "eid": i.eid_string(),
        "gid": i.gid_string(),
        "further_action": i.further_action,
        "further_action_name": further_action::name(i.further_action),
        "sync_status": i.sync_status,
        "sync_status_name": i.sync_status.map(sync_status::name),
    })
}

fn print_entity(e: &DiscoveredEntity) {
    let i = &e.info;
    let vin = i.vin_string();
    println!(
        "{}  logical {}  VIN {}  EID {}  GID {}",
        e.addr.ip(),
        hex16(i.logical_address),
        if vin.is_empty() { "(none)" } else { &vin },
        i.eid_string(),
        i.gid_string()
    );
    let sync = i
        .sync_status
        .map(|s| format!(", VIN/GID {} (0x{s:02X})", sync_status::name(s)))
        .unwrap_or_default();
    println!(
        "    further action {} (0x{:02X}){sync}",
        further_action::name(i.further_action),
        i.further_action
    );
}

fn summary(d: &Discovery) -> String {
    let mut parts = vec![format!(
        "{} entit{}",
        d.entities.len(),
        if d.entities.len() == 1 { "y" } else { "ies" }
    )];
    if d.duplicates > 0 {
        parts.push(format!("{} repeat(s) folded", d.duplicates));
    }
    if d.rejected > 0 {
        parts.push(format!("{} malformed datagram(s) dropped", d.rejected));
    }
    if d.ignored > 0 {
        parts.push(format!("{} other message(s) ignored", d.ignored));
    }
    for (from, code) in &d.nacks {
        parts.push(format!(
            "NACK 0x{code:02X} ({}) from {from}",
            generic_nack::name(*code)
        ));
    }
    parts.join("; ")
}

fn discover(a: DiscoverArgs) -> Result<()> {
    let net = &a.net;
    let sim = if net.sim {
        Some(start_sim(net.sim_fault.as_deref(), a.ipv6)?)
    } else {
        None
    };
    let tester = u16::try_from(parse_num(&a.tester)?).map_err(|_| anyhow!("--tester is 16-bit"))?;
    let tcp_port = sim
        .as_ref()
        .map_or(a.tcp_port, |s| s.tcp.local_addr().port());

    let found = if a.listen {
        let bind = match (&sim, a.bind) {
            (Some(_), _) => SocketAddr::new(
                if a.ipv6 {
                    Ipv6Addr::LOCALHOST.into()
                } else {
                    Ipv4Addr::LOCALHOST.into()
                },
                0,
            ),
            (None, Some(b)) => b,
            (None, None) if a.ipv6 => (Ipv6Addr::UNSPECIFIED, net.port).into(),
            (None, None) => (Ipv4Addr::UNSPECIFIED, net.port).into(),
        };
        let listener = DoIpUdpClient::listener(bind)
            .with_context(|| format!("listen for DoIP announcements on {bind}"))?;
        if let Some(s) = &sim {
            // The simulated entity "powers up": three announcements.
            s.udp
                .announce(listener.local_addr()?, 3, Duration::from_millis(20))?;
        }
        if !net.json {
            eprintln!(
                "listening for DoIP vehicle announcements on {} for {} ms…",
                listener.local_addr()?,
                net.timeout_ms
            );
        }
        listener.listen(net.timeout_ms)?
    } else {
        let req = match (&a.eid, &a.vin) {
            (Some(e), _) => VehicleIdRequest::by_eid(e)?,
            (_, Some(v)) => VehicleIdRequest::by_vin(v)?,
            _ => VehicleIdRequest::All,
        };
        let dest = match &sim {
            Some(s) => s.udp.local_addr()?,
            None => {
                let ip = net.ip.unwrap_or(if a.ipv6 {
                    IpAddr::V6(IPV6_ALL_NODES)
                } else {
                    IpAddr::V4(IPV4_BROADCAST)
                });
                dest_addr(ip, net.port, net.scope_id)
            }
        };
        let mut client = DoIpUdpClient::for_destination(dest)?;
        client.set_protocol_version(version(net)?);
        if !net.json {
            eprintln!(
                "vehicle identification request to {dest}, waiting {} ms…",
                net.timeout_ms
            );
        }
        if is_group(dest.ip()) {
            client.identify(dest, &req, net.timeout_ms)?
        } else {
            client.identify_first(dest, &req, net.timeout_ms)?
        }
    };

    let mut activations = Vec::new();
    if a.activate {
        for e in &found.entities {
            let opts = ConnectOptions {
                tcp_port,
                tester_address: tester,
                timeout_ms: net.timeout_ms,
                ..ConnectOptions::default()
            };
            activations.push(match connect_entity(e, &opts) {
                Ok(tp) => Ok((tp.entity_address(), tp.activation_confirmation_required())),
                Err(err) => Err(err.to_string()),
            });
        }
    }

    if net.json {
        let entities: Vec<Value> = found
            .entities
            .iter()
            .enumerate()
            .map(|(n, e)| {
                let mut v = entity_json(e);
                if let Some(r) = activations.get(n) {
                    v["routing_activation"] = match r {
                        Ok((addr, confirm)) => json!({
                            "ok": true,
                            "entity_address": addr.map(hex16),
                            "confirmation_required": confirm,
                        }),
                        Err(msg) => json!({"ok": false, "error": msg}),
                    };
                }
                v
            })
            .collect();
        let nacks: Vec<Value> = found
            .nacks
            .iter()
            .map(|(from, c)| json!({"from": from.to_string(), "code": c, "name": generic_nack::name(*c)}))
            .collect();
        let out = json!({
            "simulated": sim.is_some(),
            "entities": entities,
            "duplicates": found.duplicates,
            "rejected": found.rejected,
            "ignored": found.ignored,
            "nacks": nacks,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if sim.is_some() {
            println!("(simulated DoIP entity on loopback — synthetic data)");
        }
        for (n, e) in found.entities.iter().enumerate() {
            print_entity(e);
            match activations.get(n) {
                Some(Ok((addr, confirm))) => println!(
                    "    routing activation: ok{}{}",
                    addr.map(|a| format!(", entity {}", hex16(a)))
                        .unwrap_or_default(),
                    if *confirm {
                        ", confirmation required"
                    } else {
                        ""
                    }
                ),
                Some(Err(msg)) => println!("    routing activation: FAILED: {msg}"),
                None => {}
            }
        }
        println!("{}", summary(&found));
    }
    if found.entities.is_empty() {
        if !net.json {
            eprintln!("no DoIP entity answered within {} ms", net.timeout_ms);
        }
        return Err(crate::ExitStatus(1).into());
    }
    if activations.iter().any(|r| r.is_err()) {
        return Err(crate::ExitStatus(1).into());
    }
    Ok(())
}

/// Resolve the single entity `status` / `power` talk to.
fn target(net: &NetArgs) -> Result<(Option<Sim>, DoIpUdpClient, SocketAddr)> {
    let sim = if net.sim {
        Some(start_sim(net.sim_fault.as_deref(), false)?)
    } else {
        None
    };
    let dest = match &sim {
        Some(s) => s.udp.local_addr()?,
        None => {
            let Some(ip) = net.ip else {
                bail!("--ip is required (or --sim)");
            };
            dest_addr(ip, net.port, net.scope_id)
        }
    };
    let mut client = DoIpUdpClient::for_destination(dest)?;
    client.set_protocol_version(version(net)?);
    Ok((sim, client, dest))
}

fn status(net: NetArgs) -> Result<()> {
    let (_sim, client, dest) = target(&net)?;
    let s = client
        .entity_status(dest, net.timeout_ms)
        .with_context(|| format!("DoIP entity status from {dest}"))?;
    if net.json {
        let out = json!({
            "addr": dest.to_string(),
            "node_type": s.node_type,
            "node_type_name": node_type::name(s.node_type),
            "max_open_sockets": s.max_open_sockets,
            "open_sockets": s.open_sockets,
            "max_data_size": s.max_data_size,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!("DoIP entity {dest}");
        println!(
            "  node type:        {} (0x{:02X})",
            node_type::name(s.node_type),
            s.node_type
        );
        println!(
            "  TCP sockets:      {} open of {} max",
            s.open_sockets, s.max_open_sockets
        );
        match s.max_data_size {
            Some(m) => println!("  max data size:    {m} bytes"),
            None => println!("  max data size:    (not reported)"),
        }
    }
    Ok(())
}

fn power(net: NetArgs) -> Result<()> {
    let (_sim, client, dest) = target(&net)?;
    let p = client
        .power_mode(dest, net.timeout_ms)
        .with_context(|| format!("DoIP diagnostic power mode from {dest}"))?;
    if net.json {
        let out = json!({"addr": dest.to_string(), "power_mode": p.to_byte(), "name": p.name()});
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        println!(
            "DoIP entity {dest}: diagnostic power mode {} (0x{:02X})",
            p.name(),
            p.to_byte()
        );
    }
    Ok(())
}
