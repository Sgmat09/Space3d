//! CIDR ranges and trusted-proxy client-IP resolution.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// An IPv4/IPv6 range (`10.0.0.0/8`, `2001:db8::/32`, or a single address).
///
/// Serializes as its string form (`"10.0.0.0/8"`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cidr {
    /// Network address (host bits zeroed).
    pub addr: IpAddr,
    /// Prefix length.
    pub prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/n`, `v6/n` or a bare address.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let addr: IpAddr = addr
            .parse()
            .map_err(|_| format!("`{s}` is not an IP address or CIDR"))?;
        let addr = canonical(addr);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("`{s}`: prefix must be 0..={max}"))?,
            None => max,
        };
        Ok(Self {
            addr: mask(addr, prefix),
            prefix,
        })
    }

    /// A single address.
    pub fn host(ip: IpAddr) -> Self {
        let ip = canonical(ip);
        Self {
            addr: ip,
            prefix: if ip.is_ipv4() { 32 } else { 128 },
        }
    }

    /// Whether `ip` is inside the range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        ip.is_ipv4() == self.addr.is_ipv4() && mask(ip, self.prefix) == self.addr
    }
}

impl std::fmt::Display for Cidr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

impl std::str::FromStr for Cidr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

impl Serialize for Cidr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cidr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// IPv4-mapped IPv6 addresses (`::ffff:1.2.3.4`) are treated as IPv4.
fn canonical(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        v4 => v4,
    }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let m = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - u32::from(prefix))
            };
            IpAddr::V4(Ipv4Addr::from(bits & m))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let m = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - u32::from(prefix))
            };
            IpAddr::V6(Ipv6Addr::from(bits & m))
        }
    }
}

/// Key for per-address limits, bans-adjacent strikes and quotas: an IPv4 address as is, an
/// IPv6 address reduced to its /64 (one subscriber usually holds a whole /64, so per-address
/// keys would hand out a fresh quota for every address in it).
pub fn rate_key(ip: IpAddr) -> IpAddr {
    match canonical(ip) {
        v4 @ IpAddr::V4(_) => v4,
        v6 => mask(v6, 64),
    }
}

/// The real client IP: the right-most `X-Forwarded-For` hop not in `trusted`, but only when the
/// TCP peer itself is trusted; otherwise the peer address.
pub fn resolve_client_ip(peer: SocketAddr, forwarded_for: Option<&str>, trusted: &[Cidr]) -> IpAddr {
    let peer_ip = canonical(peer.ip());
    let is_trusted = |ip: IpAddr| trusted.iter().any(|c| c.contains(ip));
    if !is_trusted(peer_ip) {
        return peer_ip;
    }
    let Some(header) = forwarded_for else {
        return peer_ip;
    };
    let hops: Vec<IpAddr> = header
        .split(',')
        .filter_map(|h| {
            let h = h.trim();
            h.parse::<IpAddr>()
                .ok()
                .or_else(|| h.parse::<SocketAddr>().ok().map(|s| s.ip()))
        })
        .map(canonical)
        .collect();
    hops.iter()
        .rev()
        .copied()
        .find(|ip| !is_trusted(*ip))
        .or_else(|| hops.first().copied())
        .unwrap_or(peer_ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_contains() {
        let c = Cidr::parse("10.1.2.3/8").unwrap();
        assert_eq!(c.to_string(), "10.0.0.0/8");
        assert!(c.contains("10.200.0.1".parse().unwrap()));
        assert!(!c.contains("11.0.0.1".parse().unwrap()));
        assert!(c.contains("::ffff:10.0.0.9".parse().unwrap()), "v4-mapped v6");
        let v6 = Cidr::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8:1::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));
        let host = Cidr::parse("192.168.1.7").unwrap();
        assert_eq!(host.prefix, 32);
        assert!(host.contains("192.168.1.7".parse().unwrap()));
        assert!(!host.contains("192.168.1.8".parse().unwrap()));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("nope").is_err());
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, "\"10.0.0.0/8\"");
        assert_eq!(serde_json::from_str::<Cidr>(&json).unwrap(), c);
    }

    #[test]
    fn rate_keys_group_ipv6_by_slash_64() {
        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:ffff:ffff:ffff:ffff".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(rate_key(a), rate_key(b), "same /64");
        assert_ne!(rate_key(a), rate_key(c));
        let v4: IpAddr = "203.0.113.7".parse().unwrap();
        assert_eq!(rate_key(v4), v4);
        assert_eq!(rate_key("::ffff:203.0.113.7".parse().unwrap()), v4, "v4-mapped");
    }

    #[test]
    fn forwarded_for_only_from_trusted_proxies() {
        let trusted = [Cidr::parse("10.0.0.0/8").unwrap()];
        let proxy: SocketAddr = "10.0.0.5:4000".parse().unwrap();
        let direct: SocketAddr = "203.0.113.9:4000".parse().unwrap();
        // Untrusted peer: header ignored (spoofing).
        assert_eq!(
            resolve_client_ip(direct, Some("1.2.3.4"), &trusted),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // Trusted proxy: right-most untrusted hop.
        assert_eq!(
            resolve_client_ip(proxy, Some("1.2.3.4, 198.51.100.7, 10.0.0.2"), &trusted),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
        // All hops trusted: left-most.
        assert_eq!(
            resolve_client_ip(proxy, Some("10.9.9.9, 10.0.0.2"), &trusted),
            "10.9.9.9".parse::<IpAddr>().unwrap()
        );
        // Missing / garbage header: the proxy itself.
        assert_eq!(resolve_client_ip(proxy, None, &trusted), proxy.ip());
        assert_eq!(resolve_client_ip(proxy, Some("garbage"), &trusted), proxy.ip());
    }
}
