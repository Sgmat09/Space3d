//! Process-wide rustls crypto provider (native only).

/// tokio-tungstenite builds its rustls config from the process default provider,
/// which rustls can only infer when exactly one backend is compiled in. Pin
/// aws-lc-rs (the one reqwest uses) so `wss://` keeps working if another crate
/// in the final binary also enables `ring`.
pub(crate) fn ensure_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            // Losing a race against another installer is fine: any provider works.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
    });
}
