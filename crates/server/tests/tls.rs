//! TLS serving and certificate hot reload (the SIGHUP path).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chattercrab_server::config::TlsPaths;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let name: u64 = rand::random();
        let path = std::env::temp_dir().join(format!("chattercrab-tls-{name:016x}"));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Writes a fresh self-signed `localhost` certificate and returns its DER form.
fn write_cert(dir: &Path) -> CertificateDer<'static> {
    let ck = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    std::fs::write(dir.join("fullchain.pem"), ck.cert.pem()).unwrap();
    std::fs::write(dir.join("privkey.pem"), ck.signing_key.serialize_pem()).unwrap();
    ck.cert.der().clone()
}

fn connector(trusted: &CertificateDer<'static>) -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(trusted.clone()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// Performs a TLS handshake trusting only `trusted` and fetches `/health`.
async fn health_over_tls(
    addr: std::net::SocketAddr,
    trusted: &CertificateDer<'static>,
) -> Result<String, std::io::Error> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").unwrap();
    let mut tls = connector(trusted).connect(name, tcp).await?;
    tls.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = String::new();
    tls.read_to_string(&mut response).await?;
    Ok(response)
}

#[tokio::test]
async fn serves_https_and_reloads_certificate() {
    let dir = TempDir::new();
    let first = write_cert(&dir.0);
    let paths = TlsPaths {
        cert: dir.0.join("fullchain.pem"),
        key: dir.0.join("privkey.pem"),
    };
    let server = common::start_with(|c| c.tls = Some(paths)).await;

    let response = health_over_tls(server.addr, &first).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("\"status\":\"ok\""));

    // Rotate the certificate on disk and trigger the reload (what SIGHUP does).
    let second = write_cert(&dir.0);
    server.signals.reload.notify_one();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if health_over_tls(server.addr, &second).await.is_ok() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "reload not applied");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        health_over_tls(server.addr, &first).await.is_err(),
        "old certificate must no longer be served"
    );

    // A broken key file keeps the current certificate.
    std::fs::write(dir.0.join("privkey.pem"), "garbage").unwrap();
    server.signals.reload.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(health_over_tls(server.addr, &second).await.is_ok());
}

#[tokio::test]
async fn startup_fails_on_missing_certificate() {
    let dir = TempDir::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = common::test_config();
    config.tls = Some(TlsPaths {
        cert: dir.0.join("missing.pem"),
        key: dir.0.join("missing-key.pem"),
    });
    let result = chattercrab_server::run(config, listener, Default::default()).await;
    assert!(matches!(
        result,
        Err(chattercrab_server::ServerError::Tls(_))
    ));
}
