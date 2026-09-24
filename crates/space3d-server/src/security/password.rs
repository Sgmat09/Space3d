//! Password hashing (argon2id, default params) on the compute pool — never on a tick or an
//! async worker thread.

use argon2::Argon2;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use space3d_tasks::Task;

/// Longest accepted password (argon2 cost is independent of length, but bound the input).
pub const MAX_PASSWORD_LEN: usize = 256;

/// Hash a password (PHC string, `$argon2id$v=19$...`).
pub fn hash_password(password: String) -> Task<Result<String, String>> {
    space3d_tasks::spawn_compute(move || {
        if password.len() > MAX_PASSWORD_LEN {
            return Err("password too long".to_owned());
        }
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| e.to_string())
    })
}

/// Verify a password against a PHC string.
pub fn verify_password(password: String, phc: String) -> Task<bool> {
    space3d_tasks::spawn_compute(move || {
        password.len() <= MAX_PASSWORD_LEN
            && Argon2::default()
                .verify_password(password.as_bytes(), phc.as_str())
                .is_ok()
    })
}

/// A valid PHC string of a random password: verifying against it costs the same as a real
/// check, so unknown usernames are not distinguishable by timing.
pub fn dummy_phc() -> &'static str {
    static DUMMY: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DUMMY.get_or_init(|| {
        let random = hex(&super::random_bytes::<16>());
        Argon2::default()
            .hash_password(random.as_bytes())
            .map(|h| h.to_string())
            .unwrap_or_default()
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_off_thread() {
        let phc = hash_password("correct horse".into()).block().unwrap().unwrap();
        assert!(phc.starts_with("$argon2id$"));
        assert!(verify_password("correct horse".into(), phc.clone()).block().unwrap());
        assert!(!verify_password("wrong horse".into(), phc).block().unwrap());
        assert!(!verify_password("x".into(), "not a phc".into()).block().unwrap());
        assert!(!verify_password("x".into(), dummy_phc().to_owned()).block().unwrap());
        assert!(
            hash_password("x".repeat(MAX_PASSWORD_LEN + 1))
                .block()
                .unwrap()
                .is_err()
        );
    }
}
