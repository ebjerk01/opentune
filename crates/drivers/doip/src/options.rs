//! The `doip` driver's options, parsed from a [`ConnectConfig`].
//!
//! The entity is named by `ip` (or `port`, which is what the CLI's
//! `--port` / `OPENTUNE_PORT` and the desktop's port field fill):
//!
//! ```text
//! HOST[:TCP_PORT][?key=value&key=value…]
//! ```
//!
//! `HOST` is an IPv4 / IPv6 address (`[fe80::1%3]:13400` for a scoped one) or
//! a host name; an empty host or `discover` finds the entity by UDP vehicle
//! identification first. The `?…` part carries any other option, so a single
//! port string is a complete DoIP connection (`192.0.2.10?target=0x0010`).
//! Options given separately win over the ones in the port string; unknown keys
//! in the port string are refused (they were typed for this driver), unknown
//! keys elsewhere are ignored (front ends pass generic options to every driver).
use opentune_core::protocol::doip::{routing_activation, DEFAULT_PORT};
use opentune_core::protocol::doip_discovery::{
    VehicleIdRequest, A_DOIP_CTRL_MS, IPV4_BROADCAST, IPV6_ALL_NODES,
};
use opentune_core::{ConnectConfig, Error, Result};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr, SocketAddrV6, ToSocketAddrs};

/// The usual external test equipment logical address.
pub const DEFAULT_TESTER: u16 = 0x0E00;
/// TCP connect and routing activation timeout, each (ms).
pub const DEFAULT_TIMEOUT_MS: u64 = A_DOIP_CTRL_MS;
/// Bytes per ReadMemoryByAddress request.
pub const DEFAULT_READ_CHUNK: u32 = 0x100;
/// Largest ReadMemoryByAddress chunk accepted (one UDS response stays well
/// under a typical DoIP entity's max data size).
pub const MAX_READ_CHUNK: u32 = 0x0FFF;
/// Default SecurityAccess request-seed level for block reads.
pub const DEFAULT_SEEDKEY_LEVEL: u8 = 0x11;

/// Every option key the driver understands.
pub const KEYS: &[&str] = &[
    "ip",
    "port",
    "tcp_port",
    "udp_port",
    "scope_id",
    "ipv6",
    "discover",
    "vin",
    "eid",
    "tester",
    "target",
    "activation",
    "timeout_ms",
    "discover_timeout_ms",
    "session",
    "seedkey_level",
    "security",
    "seedkey",
    "read_chunk",
];

/// How the entity is found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Locate {
    /// Dial this TCP address directly.
    Direct(SocketAddr),
    /// Send a vehicle identification request to `dest` (UDP) and connect the
    /// entity that answers.
    Discover {
        /// Unicast or broadcast / multicast destination, discovery port set.
        dest: SocketAddr,
        /// All entities, or only the one with an EID / VIN.
        request: VehicleIdRequest,
    },
}

impl Locate {
    /// Whether the destination reaches many hosts (wait the whole timeout).
    pub fn is_group(&self) -> bool {
        match self {
            Locate::Direct(_) => false,
            Locate::Discover { dest, .. } => match dest.ip() {
                IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast(),
                IpAddr::V6(v6) => v6.is_multicast(),
            },
        }
    }
}

/// The parsed options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoipOptions {
    /// Where the entity is.
    pub locate: Locate,
    /// The entity's TCP data port.
    pub tcp_port: u16,
    /// Tester (source) logical address.
    pub tester: u16,
    /// ECU (target) logical address; `None` = the entity's own (announced, or
    /// learned from routing activation).
    pub target: Option<u16>,
    /// Routing activation type; `None` = default, or central security when a
    /// discovered entity asked for it.
    pub activation: Option<u8>,
    /// TCP connect and routing activation timeout, each (ms).
    pub timeout_ms: u64,
    /// How long discovery waits for answers (ms).
    pub discover_timeout_ms: u64,
    /// Session entered before a block read (`None` = leave it).
    pub session: Option<u8>,
    /// SecurityAccess request-seed level for block reads.
    pub seedkey_level: u8,
    /// Refuse a block read without a seed-key handler.
    pub security_required: bool,
    /// Bytes per ReadMemoryByAddress request.
    pub read_chunk: u32,
}

fn cfg_err(msg: impl Into<String>) -> Error {
    Error::Config(format!("doip: {}", msg.into()))
}

fn truthy(key: &str, v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(cfg_err(format!("{key}='{v}': expected 1/0 (true/false)"))),
    }
}

/// A logical address / byte value: hex, with or without `0x` (`0E00`, `0x0E00`).
fn hex(key: &str, v: &str, max: u32) -> Result<u32> {
    let t = v.trim();
    let h = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    let n = u32::from_str_radix(h, 16)
        .map_err(|_| cfg_err(format!("{key}='{v}': expected a hex value (e.g. 0x0E00)")))?;
    if n > max {
        return Err(cfg_err(format!("{key}='{v}': larger than 0x{max:X}")));
    }
    Ok(n)
}

/// A count: decimal, or hex with `0x`.
fn num(key: &str, v: &str, max: u64) -> Result<u64> {
    let t = v.trim();
    let n = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => t.parse::<u64>().ok(),
    }
    .ok_or_else(|| cfg_err(format!("{key}='{v}': expected a number")))?;
    if n > max {
        return Err(cfg_err(format!("{key}='{v}': larger than {max}")));
    }
    Ok(n)
}

fn port_num(key: &str, v: &str) -> Result<u16> {
    let n = num(key, v, u64::from(u16::MAX))? as u16;
    if n == 0 {
        return Err(cfg_err(format!("{key}: port 0 is not a port")));
    }
    Ok(n)
}

/// `default` / `wwh-obd` / `central` (or a hex byte).
fn activation(v: &str) -> Result<u8> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "default" => routing_activation::TYPE_DEFAULT,
        "wwh-obd" | "wwh_obd" | "obd" => routing_activation::TYPE_WWH_OBD,
        "central" | "central-security" | "central_security" => {
            routing_activation::TYPE_CENTRAL_SECURITY
        }
        other => hex("activation", other, 0xFF)? as u8,
    })
}

/// `none` / `default` / `programming` / `extended` (or a hex byte).
fn session(v: &str) -> Result<Option<u8>> {
    Ok(match v.trim().to_ascii_lowercase().as_str() {
        "none" | "" => None,
        "default" => Some(0x01),
        "programming" => Some(0x02),
        "extended" => Some(0x03),
        other => Some(hex("session", other, 0x7F)? as u8),
    })
}

/// Split `HOST[:PORT][?k=v&…]` into the address part and the query options.
fn split_spec(spec: &str) -> Result<(&str, BTreeMap<String, String>)> {
    let (addr, query) = match spec.split_once('?') {
        Some((a, q)) => (a, q),
        None => (spec, ""),
    };
    let mut out = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.trim().is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let k = k.trim().to_ascii_lowercase();
        if !KEYS.contains(&k.as_str()) || k == "port" || k == "ip" {
            return Err(cfg_err(format!(
                "unknown option '{k}' in the address '{spec}' (known: {})",
                KEYS.iter()
                    .filter(|k| !matches!(**k, "ip" | "port"))
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        out.insert(k, v.trim().to_string());
    }
    Ok((addr.trim(), out))
}

/// Resolve `HOST[:PORT]`: an IP literal, `[v6%scope]:port`, or a host name.
/// Returns the address and whether a port was given.
fn resolve(host: &str, default_port: u16) -> Result<(SocketAddr, bool)> {
    if let Ok(sa) = host.parse::<SocketAddr>() {
        return Ok((sa, true));
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = bare.parse::<IpAddr>() {
        return Ok((SocketAddr::new(ip, default_port), false));
    }
    let (name, port, given) = match host.rsplit_once(':') {
        Some((n, p)) if !n.contains(':') => (n, port_num("port", p)?, true),
        _ => (host, default_port, false),
    };
    let sa = (name, port)
        .to_socket_addrs()
        .map_err(|e| cfg_err(format!("cannot resolve '{name}': {e}")))?
        .next()
        .ok_or_else(|| cfg_err(format!("'{name}' has no address")))?;
    Ok((sa, given))
}

fn with_scope(sa: SocketAddr, scope: Option<u32>) -> SocketAddr {
    match (sa, scope) {
        (SocketAddr::V6(v6), Some(s)) if v6.scope_id() == 0 => {
            SocketAddrV6::new(*v6.ip(), v6.port(), v6.flowinfo(), s).into()
        }
        _ => sa,
    }
}

impl DoipOptions {
    /// Parse `cfg`'s options (see the module docs). `cfg.tx_id` is the target
    /// logical address when `target=tx_id`.
    pub fn from_config(cfg: &ConnectConfig) -> Result<Self> {
        let given = |k: &str| {
            cfg.options
                .get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        // The address string: `ip`, else `port`.
        let spec = given("ip").or_else(|| given("port")).unwrap_or_default();
        let (host, query) = split_spec(&spec)?;
        // Separate options win over the address string's.
        let mut o = query;
        for k in KEYS.iter().filter(|k| !matches!(**k, "ip" | "port")) {
            if let Some(v) = cfg.options.get(*k) {
                o.insert((*k).to_string(), v.trim().to_string());
            }
        }
        let get = |k: &str| o.get(k).map(String::as_str);

        let tester = match get("tester") {
            Some(v) if !v.is_empty() => hex("tester", v, 0xFFFF)? as u16,
            _ => DEFAULT_TESTER,
        };
        let target = match get("target").map(|v| v.to_ascii_lowercase()) {
            None => None,
            Some(v) if v.is_empty() || v == "entity" || v == "auto" => None,
            Some(v) if v == "tx_id" || v == "tx" => {
                if cfg.tx_id > 0xFFFF {
                    return Err(cfg_err(format!(
                        "target=tx_id: request id 0x{:X} is not a 16-bit logical address",
                        cfg.tx_id
                    )));
                }
                Some(cfg.tx_id as u16)
            }
            Some(v) => Some(hex("target", &v, 0xFFFF)? as u16),
        };
        let activation = match get("activation") {
            Some(v) if !v.is_empty() => Some(activation(v)?),
            _ => None,
        };
        let timeout_ms = match get("timeout_ms") {
            Some(v) if !v.is_empty() => num("timeout_ms", v, 600_000)?.max(1),
            _ => DEFAULT_TIMEOUT_MS,
        };
        let discover_timeout_ms = match get("discover_timeout_ms") {
            Some(v) if !v.is_empty() => num("discover_timeout_ms", v, 600_000)?.max(1),
            _ => A_DOIP_CTRL_MS,
        };
        let udp_port = match get("udp_port") {
            Some(v) if !v.is_empty() => port_num("udp_port", v)?,
            _ => DEFAULT_PORT,
        };
        let scope_id = match get("scope_id") {
            Some(v) if !v.is_empty() => Some(num("scope_id", v, u64::from(u32::MAX))? as u32),
            _ => None,
        };
        let ipv6 = match get("ipv6") {
            Some(v) => truthy("ipv6", v)?,
            None => false,
        };
        let request = match (get("vin"), get("eid")) {
            (Some(v), Some(e)) if !v.is_empty() && !e.is_empty() => {
                return Err(cfg_err("give vin= or eid=, not both"));
            }
            (Some(v), _) if !v.is_empty() => Some(VehicleIdRequest::by_vin(v)?),
            (_, Some(e)) if !e.is_empty() => Some(VehicleIdRequest::by_eid(e)?),
            _ => None,
        };
        let discover = match get("discover") {
            Some(v) => Some(truthy("discover", v)?),
            None => None,
        };

        let named = !(host.is_empty() || host.eq_ignore_ascii_case("discover"));
        let (addr, port_given) = if named {
            let (sa, given) = resolve(host, DEFAULT_PORT)?;
            (Some(with_scope(sa, scope_id)), given)
        } else {
            (None, false)
        };
        let tcp_port = match get("tcp_port") {
            Some(v) if !v.is_empty() => port_num("tcp_port", v)?,
            _ => match addr {
                Some(sa) if port_given => sa.port(),
                _ => DEFAULT_PORT,
            },
        };
        let wants_discovery = discover.unwrap_or(false)
            || request.is_some()
            || host.eq_ignore_ascii_case("discover")
            || addr.is_none();
        if discover == Some(false) && addr.is_none() {
            return Err(cfg_err(
                "no entity address: set ip=… (or --port HOST), or discover=1 to find it",
            ));
        }
        let locate = if wants_discovery {
            let dest = match addr {
                Some(mut sa) => {
                    sa.set_port(udp_port);
                    sa
                }
                None if ipv6 => {
                    with_scope(SocketAddr::new(IPV6_ALL_NODES.into(), udp_port), scope_id)
                }
                None => SocketAddr::new(IPV4_BROADCAST.into(), udp_port),
            };
            Locate::Discover {
                dest,
                request: request.unwrap_or(VehicleIdRequest::All),
            }
        } else {
            let mut sa = addr.expect("named host");
            sa.set_port(tcp_port);
            Locate::Direct(sa)
        };

        let session = match get("session") {
            Some(v) => session(v)?,
            None => Some(0x03),
        };
        let seedkey_level = match get("seedkey_level") {
            Some(v) if !v.is_empty() => {
                let l = hex("seedkey_level", v, 0x7F)? as u8;
                if l % 2 == 0 {
                    return Err(cfg_err(format!(
                        "seedkey_level 0x{l:02X}: a request-seed level is odd"
                    )));
                }
                l
            }
            _ => DEFAULT_SEEDKEY_LEVEL,
        };
        let security_required = get("seedkey").is_some_and(|v| !v.is_empty())
            || get("security").is_some_and(|v| v.eq_ignore_ascii_case("required"));
        let read_chunk = match get("read_chunk") {
            Some(v) if !v.is_empty() => {
                let n = num("read_chunk", v, u64::from(MAX_READ_CHUNK))? as u32;
                if n == 0 {
                    return Err(cfg_err("read_chunk must be at least 1"));
                }
                n
            }
            _ => DEFAULT_READ_CHUNK,
        };

        Ok(DoipOptions {
            locate,
            tcp_port,
            tester,
            target,
            activation,
            timeout_ms,
            discover_timeout_ms,
            session,
            seedkey_level,
            security_required,
            read_chunk,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn cfg(pairs: &[(&str, &str)]) -> ConnectConfig {
        let mut c = ConnectConfig::default();
        for (k, v) in pairs {
            c.options.insert((*k).into(), (*v).into());
        }
        c
    }

    fn parse(pairs: &[(&str, &str)]) -> DoipOptions {
        DoipOptions::from_config(&cfg(pairs)).unwrap()
    }

    fn err(pairs: &[(&str, &str)]) -> String {
        DoipOptions::from_config(&cfg(pairs))
            .unwrap_err()
            .to_string()
    }

    const V4: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

    #[test]
    fn a_bare_ip_dials_tcp_13400_with_defaults() {
        let o = parse(&[("ip", "192.0.2.10")]);
        assert_eq!(o.locate, Locate::Direct((V4, 13400).into()));
        assert_eq!(o.tester, 0x0E00);
        assert_eq!(o.target, None);
        assert_eq!(o.activation, None);
        assert_eq!(o.session, Some(0x03));
        assert_eq!(o.read_chunk, DEFAULT_READ_CHUNK);
        assert!(!o.security_required);
        // `port` is the same address field (CLI --port / OPENTUNE_PORT).
        assert_eq!(parse(&[("port", "192.0.2.10")]), o);
        // `ip` wins over `port`.
        assert_eq!(parse(&[("ip", "192.0.2.10"), ("port", "192.0.2.99")]), o);
    }

    #[test]
    fn the_port_string_carries_port_and_options() {
        let o = parse(&[(
            "port",
            "192.0.2.10:13401?target=0x0010&tester=0F00&activation=central",
        )]);
        assert_eq!(o.locate, Locate::Direct((V4, 13401).into()));
        assert_eq!(o.tcp_port, 13401);
        assert_eq!(o.target, Some(0x0010));
        assert_eq!(o.tester, 0x0F00);
        assert_eq!(o.activation, Some(0xE0));
        // A separate option wins over the port string.
        let o = parse(&[("port", "192.0.2.10?target=0x0010"), ("target", "0x0020")]);
        assert_eq!(o.target, Some(0x0020));
        // tcp_port overrides an embedded port.
        let o = parse(&[("port", "192.0.2.10:13401"), ("tcp_port", "13402")]);
        assert_eq!(o.locate, Locate::Direct((V4, 13402).into()));
        // Unknown keys in the port string are typos: refused.
        let e = err(&[("port", "192.0.2.10?traget=1")]);
        assert!(e.contains("unknown option 'traget'"), "{e}");
        // Unknown separate options belong to the front end: ignored.
        parse(&[("ip", "192.0.2.10"), ("base", "0x0"), ("coding_store", "x")]);
    }

    #[test]
    fn ipv6_literals_and_scopes() {
        let o = parse(&[("ip", "[fe80::10%3]:13400")]);
        let Locate::Direct(SocketAddr::V6(v6)) = o.locate else {
            panic!("{o:?}")
        };
        assert_eq!(v6.scope_id(), 3);
        let o = parse(&[("ip", "fe80::10"), ("scope_id", "4")]);
        let Locate::Direct(SocketAddr::V6(v6)) = o.locate else {
            panic!("{o:?}")
        };
        assert_eq!((v6.scope_id(), v6.port()), (4, 13400));
        let o = parse(&[("discover", "1"), ("ipv6", "1"), ("scope_id", "2")]);
        let Locate::Discover {
            dest: SocketAddr::V6(d),
            ..
        } = o.locate
        else {
            panic!("{o:?}")
        };
        assert_eq!(
            (*d.ip(), d.scope_id()),
            (Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1), 2)
        );
    }

    #[test]
    fn discovery_modes() {
        // No address at all: broadcast vehicle identification.
        let o = parse(&[]);
        assert_eq!(
            o.locate,
            Locate::Discover {
                dest: (Ipv4Addr::BROADCAST, 13400).into(),
                request: VehicleIdRequest::All
            }
        );
        assert!(o.locate.is_group());
        assert_eq!(parse(&[("port", "discover")]).locate, o.locate);
        // An address + discover=1: unicast identification on the UDP port.
        let o = parse(&[
            ("ip", "192.0.2.10"),
            ("discover", "1"),
            ("udp_port", "13499"),
        ]);
        assert_eq!(
            o.locate,
            Locate::Discover {
                dest: (V4, 13499).into(),
                request: VehicleIdRequest::All
            }
        );
        assert!(!o.locate.is_group());
        // vin= / eid= select and imply discovery.
        let o = parse(&[("port", "discover?vin=WVWZZZ00000000000")]);
        let Locate::Discover { request, .. } = o.locate else {
            panic!()
        };
        assert_eq!(
            request,
            VehicleIdRequest::by_vin("WVWZZZ00000000000").unwrap()
        );
        let o = parse(&[("eid", "02:00:00:00:00:10")]);
        let Locate::Discover { request, .. } = o.locate else {
            panic!()
        };
        assert_eq!(request, VehicleIdRequest::ByEid([2, 0, 0, 0, 0, 0x10]));
        // Refusals.
        assert!(err(&[("discover", "0")]).contains("no entity address"));
        assert!(err(&[("vin", "WVWZZZ00000000000"), ("eid", "020000000010")]).contains("not both"));
        assert!(!err(&[("vin", "SHORT")]).is_empty());
    }

    #[test]
    fn target_from_the_request_id() {
        let mut c = cfg(&[("ip", "192.0.2.10"), ("target", "tx_id")]);
        c.tx_id = 0x4010;
        assert_eq!(DoipOptions::from_config(&c).unwrap().target, Some(0x4010));
        c.tx_id = 0x18DA_10F1;
        let e = DoipOptions::from_config(&c).unwrap_err().to_string();
        assert!(e.contains("not a 16-bit logical address"), "{e}");
    }

    #[test]
    fn read_and_security_options() {
        let o = parse(&[
            ("ip", "192.0.2.10"),
            ("session", "none"),
            ("read_chunk", "0x400"),
            ("seedkey_level", "0x01"),
            ("security", "required"),
        ]);
        assert_eq!(o.session, None);
        assert_eq!(o.read_chunk, 0x400);
        assert_eq!(o.seedkey_level, 0x01);
        assert!(o.security_required);
        assert_eq!(
            parse(&[("ip", "192.0.2.10"), ("session", "programming")]).session,
            Some(2)
        );
        assert!(parse(&[("ip", "192.0.2.10"), ("seedkey", "x")]).security_required);
        for (k, v, want) in [
            ("seedkey_level", "0x12", "odd"),
            ("read_chunk", "0", "at least 1"),
            ("read_chunk", "5000", "larger than"),
            ("tester", "0x1FFFF", "larger than"),
            ("target", "zz", "hex value"),
            ("tcp_port", "0", "port 0"),
            ("ipv6", "maybe", "expected 1/0"),
            ("timeout_ms", "soon", "expected a number"),
        ] {
            let e = err(&[("ip", "192.0.2.10"), (k, v)]);
            assert!(e.contains(want), "{k}={v}: {e}");
        }
    }
}
