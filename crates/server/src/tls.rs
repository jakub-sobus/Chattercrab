//! Loading rustls server configurations from PEM files.

use std::path::Path;
use std::sync::Arc;

use rustls::ServerConfig;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Loading the certificate or key failed.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// The certificate file is unreadable or not PEM.
    #[error("cannot read certificate {path}: {source}")]
    Cert {
        /// File path.
        path: String,
        /// Underlying error.
        source: rustls::pki_types::pem::Error,
    },
    /// The certificate file contains no certificate.
    #[error("no certificate found in {0}")]
    NoCert(String),
    /// The key file is unreadable or not PEM.
    #[error("cannot read private key {path}: {source}")]
    Key {
        /// File path.
        path: String,
        /// Underlying error.
        source: rustls::pki_types::pem::Error,
    },
    /// rustls rejected the certificate/key pair.
    #[error("invalid TLS configuration: {0}")]
    Rustls(#[from] rustls::Error),
}

/// Reads a PEM certificate chain and private key and builds a rustls server
/// configuration using the `ring` provider, TLS 1.2/1.3 and ALPN `http/1.1`.
pub fn load_server_config(cert: &Path, key: &Path) -> Result<Arc<ServerConfig>, TlsError> {
    let cert_err = |source| TlsError::Cert {
        path: cert.display().to_string(),
        source,
    };
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert)
        .map_err(cert_err)?
        .collect::<Result<_, _>>()
        .map_err(cert_err)?;
    if certs.is_empty() {
        return Err(TlsError::NoCert(cert.display().to_string()));
    }
    let key = PrivateKeyDer::from_pem_file(key).map_err(|source| TlsError::Key {
        path: key.display().to_string(),
        source,
    })?;

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
