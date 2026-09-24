//! Security building blocks: signed tokens, passwords, rate limits, bans, client-IP resolution.

pub mod bans;
pub mod netaddr;
#[cfg(feature = "password-auth")]
pub mod password;
pub mod ratelimit;
pub mod token;

/// Fill `buf` from the OS CSPRNG (`getrandom`). Panics only if the OS has no entropy source.
pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).expect("OS random source unavailable");
    buf
}

/// Constant-time equality for secrets (bearer tokens, hashes).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    space3d_replication::server::handshake::ct_eq(a, b)
}
