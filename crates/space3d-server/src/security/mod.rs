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

use crate::config::{ConfigError, Env, Role, ServerConfig};
use std::sync::Arc;
use token::KeyRing;

/// Dev-only fleet token (refused in production).
pub const DEV_FLEET_TOKEN: &str = "space3d-dev-fleet-token";

/// Secrets resolved from env vars / files named by `[security]` (never from the config file).
#[derive(Clone)]
pub struct Secrets {
    /// Token signing / verification keys.
    pub keys: Arc<KeyRing>,
    /// Fleet bearer token (game server ↔ player service).
    pub fleet_token: String,
    /// Admin bearer tokens (plain; hashed by the admin router).
    pub admin_tokens: Vec<String>,
}

impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secrets")
            .field("kids", &self.keys.kids())
            .field("fleet_token", &"<redacted>")
            .field("admin_tokens", &self.admin_tokens.len())
            .finish()
    }
}

fn read_secret_file(path: &std::path::Path, key: &str) -> Result<String, ConfigError> {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_owned())
        .map_err(|e| ConfigError::Invalid {
            key: key.to_owned(),
            message: format!("{}: {e}", path.display()),
        })
}

impl Secrets {
    /// Resolve secrets for `config` using `env` for variable lookups. In `env = "dev"` missing
    /// secrets fall back to public dev values (with a warning); `env = "production"` refuses
    /// missing, short or dev-default secrets.
    pub fn load(config: &ServerConfig, env: &dyn Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let s = &config.security;
        let prod = config.env == Env::Production;
        let invalid = |key: &str, message: String| ConfigError::Invalid {
            key: key.to_owned(),
            message,
        };
        let keys = if let Some(path) = &s.keys_file {
            KeyRing::from_toml_file(&s.signing_kid, path).map_err(|e| invalid("security.keys_file", e))?
        } else if let Some(spec) = env(&s.keys_env).filter(|v| !v.trim().is_empty()) {
            KeyRing::from_spec(&s.signing_kid, &spec).map_err(|e| invalid("security.keys_env", e))?
        } else if prod {
            return Err(ConfigError::MissingSecret(format!(
                "signing keys (set {} or security.keys_file)",
                s.keys_env
            )));
        } else {
            tracing::warn!("no signing keys configured: using the PUBLIC dev key ring (dev only)");
            KeyRing::dev_default()
        };
        if prod && keys.uses_dev_key() {
            return Err(invalid(
                "security.keys_env",
                "the public dev key is not allowed in production".into(),
            ));
        }
        let fleet_token = match &s.fleet_token_file {
            Some(path) => Some(read_secret_file(path, "security.fleet_token_file")?),
            None => env(&s.fleet_token_env)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
        };
        let fleet_token = match fleet_token {
            Some(t) => t,
            None if prod && config.role != Role::AllInOne => {
                return Err(ConfigError::MissingSecret(format!(
                    "fleet token (set {} or security.fleet_token_file)",
                    s.fleet_token_env
                )));
            }
            None => DEV_FLEET_TOKEN.to_owned(),
        };
        if prod && config.role != Role::AllInOne && (fleet_token.len() < 16 || fleet_token == DEV_FLEET_TOKEN) {
            return Err(invalid(
                "security.fleet_token_env",
                "fleet token must be ≥ 16 characters and not the dev default".into(),
            ));
        }
        let admin_raw = match &s.admin_token_file {
            Some(path) => Some(read_secret_file(path, "security.admin_token_file")?),
            None => env(&s.admin_token_env),
        };
        let admin_tokens: Vec<String> = admin_raw
            .unwrap_or_default()
            .split(',')
            .map(|t| t.trim().to_owned())
            .filter(|t| !t.is_empty())
            .collect();
        if prod && admin_tokens.iter().all(|t| t.len() < 16) {
            return Err(ConfigError::MissingSecret(format!(
                "admin token ≥ 16 characters (set {} or security.admin_token_file)",
                s.admin_token_env
            )));
        }
        Ok(Self {
            keys: Arc::new(keys),
            fleet_token,
            admin_tokens,
        })
    }

    /// Fixed secrets for tests (random keys, given tokens).
    pub fn for_tests(fleet_token: &str, admin_tokens: &[&str]) -> Self {
        Self {
            keys: Arc::new(KeyRing::ephemeral()),
            fleet_token: fleet_token.to_owned(),
            admin_tokens: admin_tokens.iter().map(|t| (*t).to_owned()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn dev_falls_back_production_refuses() {
        let cfg = ServerConfig::default();
        let s = Secrets::load(&cfg, &env_of(&[])).unwrap();
        assert!(s.keys.uses_dev_key());
        assert_eq!(s.fleet_token, DEV_FLEET_TOKEN);
        assert!(s.admin_tokens.is_empty());
        let mut prod = cfg.clone();
        prod.env = Env::Production;
        prod.role = Role::GameServer;
        assert!(matches!(
            Secrets::load(&prod, &env_of(&[])),
            Err(ConfigError::MissingSecret(_))
        ));
        let key = "k1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=";
        let ok = Secrets::load(
            &prod,
            &env_of(&[
                ("SPACE3D_KEYS", "k1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="),
                ("SPACE3D_FLEET_TOKEN", "a-long-fleet-token-123"),
                ("SPACE3D_ADMIN_TOKENS", "short, a-long-admin-token-xyz"),
            ]),
        )
        .unwrap();
        let _ = key;
        assert_eq!(ok.admin_tokens.len(), 2);
        assert!(!ok.keys.uses_dev_key());
        assert!(format!("{ok:?}").contains("redacted"));
        assert!(
            Secrets::load(
                &prod,
                &env_of(&[
                    ("SPACE3D_KEYS", "k1:AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE="),
                    ("SPACE3D_FLEET_TOKEN", "space3d-dev-fleet-token"),
                    ("SPACE3D_ADMIN_TOKENS", "a-long-admin-token-xyz"),
                ]),
            )
            .is_err(),
            "dev fleet token refused in production"
        );
    }
}
