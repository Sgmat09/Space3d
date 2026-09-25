//! Room-side handshake helpers, shared by the gateway (`space3d-server`) and the harness.

use crate::protocol::handshake::{Hello, Reject, RejectCode, ResumeToken};
use crate::protocol::{PacketKind, WireLimits};
use crate::wire::DecodeError;

/// Decode and structurally validate the first packet of a link (must be a `Hello`, ≤
/// `MAX_HANDSHAKE_BYTES`). Used by the gateway before authentication.
pub fn parse_hello(bytes: &[u8]) -> Result<Hello, RejectCode> {
    if bytes.len() > crate::protocol::MAX_HANDSHAKE_BYTES {
        return Err(RejectCode::Malformed);
    }
    match crate::protocol::peek(bytes) {
        Some((PacketKind::Hello, 0)) => {}
        _ => return Err(RejectCode::Malformed),
    }
    let hello: Hello = crate::wire::postcard_exact(&bytes[1..]).map_err(|_| RejectCode::Malformed)?;
    hello.validate()?;
    Ok(hello)
}

/// Route a resume: the token's room, without verifying it (the room verifies).
pub fn resume_route(token_bytes: &[u8]) -> Result<ResumeToken, DecodeError> {
    ResumeToken::decode(token_bytes)
}

/// Encode a `Reject` packet.
pub fn reject_packet(code: RejectCode, message: &str, retry_after_ms: Option<u32>) -> Vec<u8> {
    crate::protocol::encode_handshake(
        PacketKind::Reject,
        &Reject {
            code,
            message: message.chars().take(crate::protocol::handshake::MAX_REASON).collect(),
            retry_after_ms,
        },
    )
}

/// Constant-time byte comparison (no early exit), for resume secrets.
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

type SecretFn = Box<dyn FnMut() -> [u8; 16] + Send + Sync>;

/// Source of resume secrets (resource in the room `App`). `space3d-server` inserts
/// [`SecretSource::from_fn`] backed by the OS CSPRNG (`getrandom`); the harness uses
/// [`SecretSource::seeded`] for reproducible runs. If none is inserted the plugin falls back
/// to a seeded source mixed with the wall clock and logs a warning (fine for dev only).
pub struct SecretSource {
    f: parking_lot::Mutex<SecretFn>,
}

impl SecretSource {
    /// Wrap a generator (must be cryptographically secure in production).
    pub fn from_fn(f: impl FnMut() -> [u8; 16] + Send + Sync + 'static) -> Self {
        Self {
            f: parking_lot::Mutex::new(Box::new(f)),
        }
    }

    /// Deterministic, **insecure** source for tests.
    pub fn seeded(seed: u64) -> Self {
        let mut rng = crate::transport::conditioner::SplitMix64::new(seed);
        Self::from_fn(move || {
            let mut out = [0u8; 16];
            out[..8].copy_from_slice(&rng.next_u64().to_le_bytes());
            out[8..].copy_from_slice(&rng.next_u64().to_le_bytes());
            out
        })
    }

    /// Next secret.
    pub fn generate(&mut self) -> [u8; 16] {
        (self.f.get_mut())()
    }
}

/// Limits used to validate inbound packets before a Welcome (handshake only).
pub fn handshake_limits() -> WireLimits {
    WireLimits::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::encode_handshake;
    use crate::protocol::handshake::{ClientCaps, HelloAuth};

    #[test]
    fn parse_hello_checks_structure() {
        let h = Hello::new("g", "1", 7, HelloAuth::Dev { name: "a".into() }, ClientCaps::default());
        let bytes = encode_handshake(PacketKind::Hello, &h);
        assert_eq!(parse_hello(&bytes), Ok(h.clone()));
        let mut bad = h.clone();
        bad.protocol = 99;
        assert_eq!(
            parse_hello(&encode_handshake(PacketKind::Hello, &bad)),
            Err(RejectCode::VersionMismatch)
        );
        assert_eq!(parse_hello(&[0x04, 0x00]), Err(RejectCode::Malformed));
        assert_eq!(parse_hello(&vec![0x01; 5000]), Err(RejectCode::Malformed));
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert_eq!(parse_hello(&trailing), Err(RejectCode::Malformed));
    }

    #[test]
    fn ct_eq_and_secrets() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
        let mut a = SecretSource::seeded(1);
        let mut b = SecretSource::seeded(1);
        assert_eq!(a.generate(), b.generate());
        assert_ne!(a.generate(), [0; 16]);
        let r = reject_packet(RejectCode::RoomFull, &"x".repeat(300), Some(5));
        assert!(r.len() < 200);
    }
}
