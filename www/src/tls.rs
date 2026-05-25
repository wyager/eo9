//! TLS configuration: manually provided certificates and automatic ACME (Let's Encrypt)
//! certificates via TLS-ALPN-01.
//!
//! Every configuration here uses rustls with the `ring` crypto provider — no OpenSSL
//! linkage anywhere in the binary.

use std::io;
use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls_acme::caches::DirCache;
use rustls_acme::{AcmeConfig, AcmeState};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// The ALPN protocol we serve behind TLS.
const ALPN_HTTP1: &[u8] = b"http/1.1";

/// The crypto provider used for every TLS configuration in this crate.
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// TLS settings for the HTTPS listener.
pub struct TlsSettings {
    /// Configuration for ordinary connections.
    pub server_config: Arc<ServerConfig>,
    /// Configuration for TLS-ALPN-01 challenge handshakes (ACME mode only).
    pub challenge_config: Option<Arc<ServerConfig>>,
}

/// The ACME state machine; it must be polled (see `server::run`) to obtain the initial
/// certificate and to renew it before expiry. Certificates and the account key are cached
/// on disk, so restarts reuse them instead of re-ordering.
pub type AcmeDriver = AcmeState<io::Error, io::Error>;

/// Build TLS settings from a PEM certificate chain and private key (manual mode).
pub fn manual_tls(cert_path: &Path, key_path: &Path) -> Result<TlsSettings, String> {
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
        .map_err(|e| {
            format!(
                "failed to read certificate file {}: {e}",
                cert_path.display()
            )
        })?
        .collect::<Result<_, _>>()
        .map_err(|e| {
            format!(
                "failed to parse certificate file {}: {e}",
                cert_path.display()
            )
        })?;
    if certs.is_empty() {
        return Err(format!("no certificates found in {}", cert_path.display()));
    }
    let key = PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
        format!(
            "failed to read private key file {}: {e}",
            key_path.display()
        )
    })?;

    let mut config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls protocol configuration: {e}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("certificate/key pair rejected: {e}"))?;
    config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];

    Ok(TlsSettings {
        server_config: Arc::new(config),
        challenge_config: None,
    })
}

/// Build TLS settings whose certificates come from Let's Encrypt, plus the state machine
/// that obtains and renews them. `staging` selects the Let's Encrypt staging environment
/// (rate-limit-free, but its certificates are not publicly trusted).
pub fn acme_tls(
    domains: &[String],
    email: &str,
    cache_dir: &Path,
    staging: bool,
) -> Result<(TlsSettings, AcmeDriver), String> {
    let state = AcmeConfig::new(domains)
        .contact_push(format!("mailto:{email}"))
        .cache(DirCache::new(cache_dir.to_path_buf()))
        .directory_lets_encrypt(!staging)
        .state();

    let mut server_config = ServerConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls protocol configuration: {e}"))?
        .with_no_client_auth()
        .with_cert_resolver(state.resolver());
    server_config.alpn_protocols = vec![ALPN_HTTP1.to_vec()];

    let settings = TlsSettings {
        server_config: Arc::new(server_config),
        challenge_config: Some(state.challenge_rustls_config_with_provider(crypto_provider())),
    };
    Ok((settings, state))
}
