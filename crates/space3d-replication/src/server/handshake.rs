//! Room-side handshake helpers, shared by the gateway (`space3d-server`) and the harness.

use crate::protocol::handshake::{Hello, Reject, RejectCode, ResumeToken};
use crate::protocol::{PacketKind, WireLimits};
use crate::wire::DecodeError;

/// Decode and structurally validate the first packet of a link (must be a `Hello`, ≤
/// `MAX_HANDSHAKE_BYTES`). Used by the gateway before authentication.
pub fn parse_hello(bytes: &[u8]) -> Result<Hello, RejectCode> {
    todo!()
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
