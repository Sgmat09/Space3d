//! CIDR ranges and trusted-proxy client-IP resolution.

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};

/// An IPv4/IPv6 range (`10.0.0.0/8`, `2001:db8::/32`, or a single address).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Cidr {
    /// Network address.
    pub addr: IpAddr,
    /// Prefix length.
    pub prefix: u8,
}

impl Cidr {
    /// Parse `a.b.c.d/n`, `v6/n` or a bare address.
    pub fn parse(s: &str) -> Result<Self, String> {
        todo!()
    }

    /// Whether `ip` is inside the range.
    pub fn contains(&self, ip: IpAddr) -> bool {
        todo!()
    }
}

/// The real client IP: the right-most `X-Forwarded-For` hop not in `trusted`, but only when the
/// TCP peer itself is trusted; otherwise the peer address.
pub fn resolve_client_ip(peer: SocketAddr, forwarded_for: Option<&str>, trusted: &[Cidr]) -> IpAddr {
    todo!()
}
