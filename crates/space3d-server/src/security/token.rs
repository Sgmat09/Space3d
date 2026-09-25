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
use hmac::{Hmac, KeyInit, Mac};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
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

    /// Parse `kid:<base64>,kid2:<base64>` (standard or URL-safe base64, padding optional).
    pub fn from_spec(signing: &str, spec: &str) -> Result<Self, String> {
        let mut keys = BTreeMap::new();
        for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let (kid, b64) = part
                .split_once(':')
                .ok_or_else(|| "key spec must look like `kid:<base64>,kid2:<base64>`".to_owned())?;
            let kid = kid.trim();
            if kid.is_empty() || kid.contains('.') {
                return Err(format!("invalid key id `{kid}`"));
            }
            let bytes = decode_any_b64(b64.trim()).ok_or_else(|| format!("key `{kid}` is not valid base64"))?;
            keys.insert(
                kid.to_owned(),
                Secret::new(bytes).map_err(|e| format!("key `{kid}`: {e}"))?,
            );
        }
        if keys.is_empty() {
            return Err("no keys".into());
        }
        Self::new(signing, keys)
    }

    /// Parse a TOML file `{ kid = "<base64>" }`.
    pub fn from_toml_file(signing: &str, path: &std::path::Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let table: toml::Table = text.parse().map_err(|e| format!("{}: {e}", path.display()))?;
        let spec = table
            .iter()
            .map(|(k, v)| match v.as_str() {
                Some(s) => Ok(format!("{k}:{s}")),
                None => Err(format!("{}: key `{k}` must be a base64 string", path.display())),
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        Self::from_spec(signing, &spec)
    }

    /// A fixed, **publicly known** dev key ring (`kid = "dev"`), so a dev player service and
    /// dev game servers agree on keys without configuration. Refused in production.
    pub fn dev_default() -> Self {
        let mut keys = BTreeMap::new();
        keys.insert("dev".to_owned(), Secret(DEV_KEY.to_vec()));
        Self {
            signing: "dev".into(),
            keys,
        }
    }

    /// Whether any key is the public dev key.
    pub fn uses_dev_key(&self) -> bool {
        self.keys.values().any(|k| k.0 == DEV_KEY)
    }

    /// Key ids (sorted).
    pub fn kids(&self) -> Vec<String> {
        self.keys.keys().cloned().collect()
    }

    fn derived(&self, kid: &str, kind: TokenKind) -> Option<Hmac<Sha256>> {
        let master = self.keys.get(kid)?;
        let mut kdf = <Hmac<Sha256> as KeyInit>::new_from_slice(master.expose()).ok()?;
        kdf.update(b"space3d/");
        kdf.update(kind.as_str().as_bytes());
        let derived = kdf.finalize().into_bytes();
        <Hmac<Sha256> as KeyInit>::new_from_slice(&derived).ok()
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

/// A fresh random key as a `SPACE3D_KEYS` entry: `kid:<base64 32 bytes>`.
pub fn generate_key_spec(kid: &str) -> String {
    use base64::Engine;
    format!(
        "{kid}:{}",
        base64::engine::general_purpose::STANDARD.encode(super::random_bytes::<32>())
    )
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

/// The public dev key (32 bytes). Never valid in production.
const DEV_KEY: &[u8; 32] = b"space3d-dev-key-NOT-FOR-PROD!!!!";

/// Longest token accepted by [`verify`] (tickets are ≤ 1 KiB on the wire).
pub const MAX_TOKEN_LEN: usize = 4096;

fn decode_any_b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    [STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD]
        .iter()
        .find_map(|e| e.decode(s).ok())
}

fn b64url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn unb64url(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s).ok()
}

/// Sign claims with the ring's signing key.
pub fn sign<C: Claims>(ring: &KeyRing, claims: &C) -> String {
    let payload = b64url(&postcard::to_stdvec(claims).expect("claims serialize"));
    let prefix = format!("s3.{}.{}.{}", C::KIND.as_str(), ring.signing, payload);
    let mut mac = ring
        .derived(&ring.signing, C::KIND)
        .expect("signing key is always present");
    mac.update(prefix.as_bytes());
    let tag = mac.finalize().into_bytes();
    format!("{prefix}.{}", b64url(&tag))
}

/// Verify a token of kind `C::KIND` at `now_ms` with `leeway_ms`.
pub fn verify<C: Claims>(ring: &KeyRing, token: &str, now_ms: i64, leeway_ms: i64) -> Result<C, TokenError> {
    if token.len() > MAX_TOKEN_LEN || !token.is_ascii() {
        return Err(TokenError::Malformed);
    }
    let mut parts = token.split('.');
    let (Some("s3"), Some(kind), Some(kid), Some(payload), Some(tag), None) = (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) else {
        return Err(TokenError::Malformed);
    };
    if kind != C::KIND.as_str() {
        return Err(TokenError::WrongKind);
    }
    let mut mac = ring.derived(kid, C::KIND).ok_or(TokenError::UnknownKey)?;
    let tag = unb64url(tag).ok_or(TokenError::Malformed)?;
    let signed_len = token.len() - (token.len() - token.rfind('.').unwrap_or(0));
    mac.update(&token.as_bytes()[..signed_len]);
    mac.verify_slice(&tag).map_err(|_| TokenError::BadSignature)?;
    let bytes = unb64url(payload).ok_or(TokenError::Malformed)?;
    let claims: C = postcard::from_bytes(&bytes).map_err(|e| TokenError::Claims(e.to_string()))?;
    if now_ms > claims.expires_ms().saturating_add(leeway_ms) {
        return Err(TokenError::Expired);
    }
    if claims.issued_ms() > now_ms.saturating_add(leeway_ms) {
        return Err(TokenError::NotYetValid);
    }
    Ok(claims)
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
    /// Extra identity claims (rating, cosmetics) → `PlayerIdentity::extra`. Carried as JSON
    /// text inside the postcard payload (postcard cannot encode self-describing values).
    #[serde(with = "json_text")]
    pub extra: serde_json::Value,
}

/// Serde adapter: a `serde_json::Value` as JSON text (for non-self-describing formats).
pub mod json_text {
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serialize as JSON text.
    pub fn serialize<S: Serializer>(v: &serde_json::Value, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    /// Deserialize from JSON text.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<serde_json::Value, D::Error> {
        let text = String::deserialize(d)?;
        serde_json::from_str(&text).map_err(serde::de::Error::custom)
    }
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

/// Result of [`NonceCache::insert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonceCheck {
    /// First use: recorded until its expiry.
    Fresh,
    /// Seen before and not expired: a replay.
    Replayed,
    /// The cache is full of unexpired nonces: refuse (fail closed, retryable). Evicting a live
    /// nonce would let that ticket be replayed until it expires.
    Full,
}

/// Seen `jti`s until their expiry. Bounded: expired entries are pruned in expiry order
/// (O(log n) each); when `capacity` unexpired nonces are held, new tickets are refused with
/// [`NonceCheck::Full`] instead of evicting a live one.
#[derive(Debug, Default)]
pub struct NonceCache {
    seen: std::collections::HashMap<[u8; 16], i64>,
    by_expiry: std::collections::BTreeSet<(i64, [u8; 16])>,
    capacity: usize,
}

impl NonceCache {
    /// Cache holding at most `capacity` nonces.
    pub fn new(capacity: usize) -> Self {
        Self {
            seen: Default::default(),
            by_expiry: Default::default(),
            capacity,
        }
    }

    /// Record `jti` (valid until `exp_ms`) at `now_ms`.
    pub fn insert(&mut self, jti: [u8; 16], exp_ms: i64, now_ms: i64) -> NonceCheck {
        // Prune everything expired (oldest expiry first).
        while let Some(&(until, old)) = self.by_expiry.first() {
            if until >= now_ms {
                break;
            }
            self.by_expiry.pop_first();
            self.seen.remove(&old);
        }
        if self.seen.contains_key(&jti) {
            return NonceCheck::Replayed;
        }
        if self.seen.len() >= self.capacity.max(1) {
            return NonceCheck::Full;
        }
        self.seen.insert(jti, exp_ms);
        self.by_expiry.insert((exp_ms, jti));
        NonceCheck::Fresh
    }

    /// Nonces tracked.
    pub fn len(&self) -> usize {
        self.seen.len()
    }

    /// Whether empty.
    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(kid: &str, byte: u8) -> KeyRing {
        let mut keys = BTreeMap::new();
        keys.insert(kid.to_owned(), Secret::new(vec![byte; 32]).unwrap());
        KeyRing::new(kid, keys).unwrap()
    }

    fn join(exp_ms: i64) -> JoinClaims {
        JoinClaims {
            player: PlayerId(42),
            display_name: "alice".into(),
            team: Some(1),
            server_id: "gs-1".into(),
            room: RoomId(9),
            spec: RoomSpec {
                room_id: RoomId(9),
                mode: "duel".into(),
                max_players: 2,
                ..Default::default()
            },
            jti: [7; 16],
            iat_ms: 1_000,
            exp_ms,
            extra: serde_json::json!({"rating": 1500}),
        }
    }

    #[test]
    fn sign_verify_roundtrip() {
        let r = ring("k1", 1);
        let token = sign(&r, &join(61_000));
        assert!(token.starts_with("s3.join.k1."));
        let back: JoinClaims = verify(&r, &token, 30_000, 5_000).unwrap();
        assert_eq!(back, join(61_000));
    }

    #[test]
    fn tamper_expiry_and_kind_confusion() {
        let r = ring("k1", 1);
        let token = sign(&r, &join(61_000));
        // Any flipped payload character breaks the MAC.
        let mut parts: Vec<String> = token.split('.').map(str::to_owned).collect();
        let mut payload = parts[3].clone().into_bytes();
        payload[2] = if payload[2] == b'A' { b'B' } else { b'A' };
        parts[3] = String::from_utf8(payload).unwrap();
        let tampered = parts.join(".");
        assert_eq!(
            verify::<JoinClaims>(&r, &tampered, 30_000, 5_000).unwrap_err(),
            TokenError::BadSignature
        );
        assert_eq!(
            verify::<JoinClaims>(&r, "garbage", 0, 0).unwrap_err(),
            TokenError::Malformed
        );
        assert_eq!(
            verify::<JoinClaims>(&r, &format!("{token}.x"), 0, 0).unwrap_err(),
            TokenError::Malformed
        );
        // Expiry with leeway.
        assert!(verify::<JoinClaims>(&r, &token, 65_999, 5_000).is_ok());
        assert_eq!(
            verify::<JoinClaims>(&r, &token, 66_001, 5_000).unwrap_err(),
            TokenError::Expired
        );
        assert_eq!(
            verify::<JoinClaims>(&r, &token, -10_000, 5_000).unwrap_err(),
            TokenError::NotYetValid
        );
        // A session token never passes as a join ticket (different kind and derived key).
        let sess = sign(
            &r,
            &SessionClaims {
                player: PlayerId(42),
                sid: [1; 16],
                iat_ms: 0,
                exp_ms: 100_000,
            },
        );
        assert_eq!(
            verify::<JoinClaims>(&r, &sess, 1_000, 0).unwrap_err(),
            TokenError::WrongKind
        );
        let forged = sess.replacen("s3.sess.", "s3.join.", 1);
        assert_eq!(
            verify::<JoinClaims>(&r, &forged, 1_000, 0).unwrap_err(),
            TokenError::BadSignature
        );
        // Another key ring rejects it.
        assert_eq!(
            verify::<JoinClaims>(&ring("k1", 2), &token, 30_000, 5_000).unwrap_err(),
            TokenError::BadSignature
        );
        assert_eq!(
            verify::<JoinClaims>(&ring("k9", 1), &token, 30_000, 5_000).unwrap_err(),
            TokenError::UnknownKey
        );
    }

    #[test]
    fn generated_keys_parse() {
        let spec = generate_key_spec("k7");
        let ring = KeyRing::from_spec("k7", &spec).unwrap();
        assert_eq!(ring.kids(), vec!["k7".to_owned()]);
        assert_ne!(generate_key_spec("k7"), spec);
    }

    #[test]
    fn kid_rotation() {
        use base64::Engine;
        let b64 = |b: u8| base64::engine::general_purpose::STANDARD.encode([b; 32]);
        let old = KeyRing::from_spec("k1", &format!("k1:{}", b64(1))).unwrap();
        let both = KeyRing::from_spec("k2", &format!("k2:{},k1:{}", b64(2), b64(1))).unwrap();
        let new_only = KeyRing::from_spec("k2", &format!("k2:{}", b64(2))).unwrap();
        let t_old = sign(&old, &join(61_000));
        let t_new = sign(&both, &join(61_000));
        assert!(t_new.starts_with("s3.join.k2."));
        assert!(
            verify::<JoinClaims>(&both, &t_old, 1_000, 0).is_ok(),
            "old tokens verify during rotation"
        );
        assert!(verify::<JoinClaims>(&new_only, &t_new, 1_000, 0).is_ok());
        assert_eq!(
            verify::<JoinClaims>(&new_only, &t_old, 1_000, 0).unwrap_err(),
            TokenError::UnknownKey
        );
        assert!(
            KeyRing::from_spec("k1", "k1:c2hvcnQ=").is_err(),
            "short keys are refused"
        );
        assert!(
            KeyRing::from_spec("k3", &format!("k1:{}", b64(1))).is_err(),
            "signing kid must exist"
        );
        assert!(!both.uses_dev_key());
        assert!(KeyRing::dev_default().uses_dev_key());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys.toml");
        std::fs::write(&path, format!("k1 = \"{}\"\nk2 = \"{}\"\n", b64(1), b64(2))).unwrap();
        let from_file = KeyRing::from_toml_file("k2", &path).unwrap();
        assert_eq!(from_file.kids(), vec!["k1".to_owned(), "k2".to_owned()]);
        assert!(verify::<JoinClaims>(&from_file, &t_old, 1_000, 0).is_ok());
    }

    #[test]
    fn nonce_cache_detects_replay_and_stays_bounded() {
        let mut c = NonceCache::new(3);
        assert_eq!(c.insert([1; 16], 100, 0), NonceCheck::Fresh);
        assert_eq!(c.insert([1; 16], 100, 50), NonceCheck::Replayed);
        assert_eq!(c.insert([2; 16], 200, 50), NonceCheck::Fresh);
        assert_eq!(c.insert([3; 16], 300, 50), NonceCheck::Fresh);
        assert_eq!(
            c.insert([4; 16], 400, 150),
            NonceCheck::Fresh,
            "expired [1] is purged to make room"
        );
        assert_eq!(c.len(), 3);
        assert_eq!(c.insert([4; 16], 400, 160), NonceCheck::Replayed);
        // Full of live nonces: a new ticket is refused, and no live nonce is evicted.
        assert_eq!(c.insert([5; 16], 500, 160), NonceCheck::Full);
        assert_eq!(c.insert([2; 16], 200, 170), NonceCheck::Replayed, "live nonce kept");
        assert_eq!(c.len(), 3);
        // Once one expires there is room again.
        assert_eq!(c.insert([5; 16], 500, 201), NonceCheck::Fresh);
    }
}
