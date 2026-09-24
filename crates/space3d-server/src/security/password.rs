//! Password hashing (argon2id, default params) on the compute pool — never on a tick or an
//! async worker thread.

use space3d_tasks::Task;

/// Hash a password (PHC string).
pub fn hash_password(password: String) -> Task<Result<String, String>> {
    todo!("spawn_compute + argon2::Argon2::default().hash_password with a random salt")
}

/// Verify a password against a PHC string.
pub fn verify_password(password: String, phc: String) -> Task<bool> {
    todo!()
}
