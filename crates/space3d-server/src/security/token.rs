//! Compact HMAC-SHA256 tokens (no JWT: no algorithm confusion, binary claims, smaller).
//!
//! ```text
//! token   = "s3." kind "." kid "." b64url(postcard(claims)) "." b64url(mac)
//! mac     = HMAC-SHA256(K_kind, "s3." kind "." kid "." payload)
//! K_kind  = HMAC-SHA256(master[kid], "space3d/" kind)     // domain separation: a Session token
//!                                                         // can never pass as a Join ticket
//! ```
//!
//! MACs are checked with `Mac::verify_slice` (constant time). `exp`/`iat` are checked with a
//! leeway (default 5 s). Rotation: add `k2` everywhere, switch `signing_kid` on the player
//! service, remove `k1` after the longest token TTL (tickets: 60 s; sessions: 1 h).

use crate::game::RoomSpec;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use space3d_replication::{PlayerId, RoomId};
use std::collections::BTreeMap;

/// Token kinds (each has its own derived key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    /// Access/session token (`Authorization: Bearer`).
    Session,
    /// Join ticket (in `Hello`).
    Join,
}

impl TokenKind {
    /// Wire name (`"sess"`, `"join"`).
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Session => "sess",
            TokenKind::Join => "join",
        }
    }
}

/// Token verification failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// Not `s3.<kind>.<kid>.<payload>.<mac>`.
    #[error("malformed token")]
    Malformed,
    /// Kind differs from the expected one.
    #[error("wrong token kind")]
    WrongKind,
    /// `kid` not in the key ring.
    #[error("unknown key id")]
    UnknownKey,
    /// MAC mismatch.
    #[error("bad signature")]
    BadSignature,
    /// Past `exp` (+ leeway).
    #[error("expired")]
    Expired,
    /// `iat` in the future (− leeway).
    #[error("not yet valid")]
    NotYetValid,
    /// Claims failed to decode.
    #[error("claims: {0}")]
    Claims(String),
}

/// A master secret (≥ 32 bytes). Debug never prints it.
#[derive(Clone)]
pub struct Secret(Vec<u8>);

impl Secret {
    /// Wrap bytes (`Err` if shorter than 32).
    pub fn new(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() < 32 {
            return Err(format!("secret must be at least 32 bytes, got {}", bytes.len()));
        }
        Ok(Self(bytes))
    }

    /// Raw bytes.
    pub fn expose(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Signing/verification keys by id.
#[derive(Clone, Debug)]
pub struct KeyRing {
    signing: String,
    keys: BTreeMap<String, Secret>,
}

impl KeyRing {
    /// Ring that signs with `signing` (must be present in `keys`).
    pub fn new(signing: &str, keys: BTreeMap<String, Secret>) -> Result<Self, String> {
        if !keys.contains_key(signing) {
            return Err(format!("signing key `{signing}` not in key ring"));
        }
        Ok(Self {
            signing: signing.to_owned(),
            keys,
        })
    }

    /// Parse `kid:<base64>,kid2:<base64>`.
    pub fn from_spec(signing: &str, spec: &str) -> Result<Self, String> {
        todo!()
    }

    /// Parse a TOML file `{ kid = "<base64>" }`.
    pub fn from_toml_file(signing: &str, path: &std::path::Path) -> Result<Self, String> {
        todo!()
    }

    /// Random single-key ring (dev only; tokens die with the process).
    pub fn ephemeral() -> Self {
        let mut keys = BTreeMap::new();
        keys.insert("dev".to_owned(), Secret(super::random_bytes::<32>().to_vec()));
        Self {
            signing: "dev".into(),
            keys,
        }
    }

    /// Key id used for signing.
    pub fn signing_kid(&self) -> &str {
        &self.signing
    }
}

/// Claims carried by a token kind.
pub trait Claims: Serialize + DeserializeOwned {
    /// Kind (selects the derived key).
    const KIND: TokenKind;
    /// Expiry (unix ms).
    fn expires_ms(&self) -> i64;
    /// Issue time (unix ms).
    fn issued_ms(&self) -> i64;
}

/// Sign claims with the ring's signing key.
pub fn sign<C: Claims>(ring: &KeyRing, claims: &C) -> String {
    todo!()
}

/// Verify a token of kind `C::KIND` at `now_ms` with `leeway_ms`.
pub fn verify<C: Claims>(ring: &KeyRing, token: &str, now_ms: i64, leeway_ms: i64) -> Result<C, TokenError> {
    todo!()
}

/// Join ticket claims (issued by the player service, verified offline by the game server).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JoinClaims {
    /// Account.
    pub player: PlayerId,
    /// Display name.
    pub display_name: String,
    /// Team from matchmaking.
    pub team: Option<u16>,
    /// Game server the ticket is bound to (`WrongServer` otherwise).
    pub server_id: String,
    /// Room.
    pub room: RoomId,
    /// Room to create if it does not exist yet (lazy creation).
    pub spec: RoomSpec,
    /// Single-use nonce (`TicketReused` on replay).
    pub jti: [u8; 16],
    /// Issued (unix ms).
    pub iat_ms: i64,
    /// Expires (unix ms; iat + 60 s).
    pub exp_ms: i64,
    /// Extra identity claims (rating, cosmetics) → `PlayerIdentity::extra`.
    pub extra: serde_json::Value,
}

impl Claims for JoinClaims {
    const KIND: TokenKind = TokenKind::Join;
    fn expires_ms(&self) -> i64 {
        self.exp_ms
    }
    fn issued_ms(&self) -> i64 {
        self.iat_ms
    }
}

/// Session (access token) claims. Stateless; refresh tokens are opaque and stored hashed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionClaims {
    /// Account.
    pub player: PlayerId,
    /// Session id (refresh family), for revocation checks through a 30 s cache.
    pub sid: [u8; 16],
    /// Issued.
    pub iat_ms: i64,
    /// Expires.
    pub exp_ms: i64,
}

impl Claims for SessionClaims {
    const KIND: TokenKind = TokenKind::Session;
    fn expires_ms(&self) -> i64 {
        self.exp_ms
    }
    fn issued_ms(&self) -> i64 {
        self.iat_ms
    }
}

/// Seen `jti`s until their expiry (bounded; oldest evicted first — eviction can only let an
/// already-expired ticket through the cache, which the `exp` check then rejects).
#[derive(Debug, Default)]
pub struct NonceCache {
    seen: std::collections::HashMap<[u8; 16], i64>,
    capacity: usize,
}

impl NonceCache {
    /// Cache holding at most `capacity` nonces.
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: Default::default(),
            capacity,
        }
    }

    /// Record `jti` (valid until `exp_ms`); `false` if it was already seen.
    pub fn insert(&mut self, jti: [u8; 16], exp_ms: i64, now_ms: i64) -> bool {
        todo!()
    }
}
