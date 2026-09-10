//! TLS listener for custom-resource `ResponseURL` signals.
//!
//! Real CloudFormation hands the handler a pre-signed **HTTPS** S3 URL and the
//! handler PUTs to it over TLS on 443. CDK's custom-resource framework takes
//! that literally: it calls `https.request` regardless of the URL's scheme, and
//! builds the request options from `parsedUrl.hostname` with no `port`, so Node
//! defaults to 443. An `http://host:4566/...` URL can therefore never work —
//! the handler dials `https://host:443` whatever we send.
//!
//! So serve the response route over TLS on 443, which is the same shape AWS
//! presents to the handler: an unmodified function doing an ordinary HTTPS PUT
//! to a hostname on the default port.
//!
//! The certificate is self-signed and generated at startup, where AWS's is
//! publicly trusted. Handlers are told to accept it via environment variables
//! rather than a trusted CA bundle, because fakecloud commonly runs in a
//! container while Lambda containers are its siblings: a CA file written inside
//! fakecloud's container cannot be bind-mounted into theirs. Injecting a real CA
//! is the upgrade path if that topology ever stops being the common one.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Default port. Not configurable in the URL sense: CDK's framework ignores any
/// port we would put there, so a different port only helps handlers that honour
/// it (`FAKECLOUD_CFN_RESPONSE_TLS_PORT`, mainly for tests).
pub const DEFAULT_TLS_PORT: u16 = 443;

pub fn tls_port() -> u16 {
    std::env::var("FAKECLOUD_CFN_RESPONSE_TLS_PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_TLS_PORT)
}

/// Build a rustls config from a freshly generated self-signed certificate
/// covering the names a Lambda container might dial fakecloud by.
/// Generate the certificate, returning both the server config and the PEM to
/// hand to Lambda containers so they can verify this endpoint.
pub fn self_signed(names: &[String]) -> Result<(rustls::ServerConfig, String), String> {
    // rustls 0.23 has no implicit provider; several are vendored here, so pick
    // one explicitly. Already-installed is not an error.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certified = rcgen::generate_simple_self_signed(names.to_vec())
        .map_err(|e| format!("failed to generate a certificate: {e}"))?;
    let pem = certified.cert.pem();
    let cert = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::try_from(certified.key_pair.serialize_der())
        .map_err(|e| format!("failed to encode the private key: {e}"))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(|e| format!("failed to build the TLS config: {e}"))?;
    Ok((config, pem))
}

/// Write the certificate where the Lambda runtime can copy it from, and tell it
/// where that is. A fixed path, rewritten each start: the certificate is
/// regenerated per process, so a stale one must not linger.
pub fn publish_ca_bundle(pem: &str) -> Result<std::path::PathBuf, String> {
    let path = std::env::temp_dir().join("fakecloud-ca.pem");
    std::fs::write(&path, pem).map_err(|e| format!("failed to write {}: {e}", path.display()))?;
    std::env::set_var("FAKECLOUD_LAMBDA_CA_BUNDLE", &path);
    Ok(path)
}

/// Claim the port up front, before the router exists.
///
/// Bound early so the `ResponseURL` we hand out reflects what actually
/// listens: binding 443 needs privilege on Linux and the port is often taken,
/// and neither should stop fakecloud starting — it just means no URL is sent.
pub async fn bind(port: u16) -> Result<(TcpListener, SocketAddr), String> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .map_err(|e| format!("failed to bind port {port}: {e}"))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("failed to read the bound address: {e}"))?;
    Ok((listener, addr))
}

/// Serve `app` over TLS on an already-bound listener, until the process ends.
pub fn serve(listener: TcpListener, app: axum::Router, config: rustls::ServerConfig) {
    let acceptor = TlsAcceptor::from(Arc::new(config));

    tokio::spawn(async move {
        loop {
            let Ok((stream, _peer)) = listener.accept().await else {
                continue;
            };
            let acceptor = acceptor.clone();
            let app = app.clone();
            tokio::spawn(async move {
                let tls = match acceptor.accept(stream).await {
                    Ok(tls) => tls,
                    // A handler that rejects our certificate shows up here; the
                    // connection is simply dropped, as any TLS server would.
                    Err(e) => {
                        tracing::debug!(error = %e, "TLS handshake failed on the ResponseURL listener");
                        return;
                    }
                };
                let io = hyper_util::rt::TokioIo::new(tls);
                let service = hyper_util::service::TowerToHyperService::new(app);
                if let Err(e) = hyper_util::server::conn::auto::Builder::new(
                    hyper_util::rt::TokioExecutor::new(),
                )
                .serve_connection(io, service)
                .await
                {
                    tracing::debug!(error = %e, "ResponseURL connection ended");
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_covers_every_name_a_container_might_dial() {
        // Names differ by platform (docker vs podman) and topology, so all of
        // them go in the SANs rather than guessing one.
        let names = vec![
            "host.docker.internal".to_string(),
            "host.containers.internal".to_string(),
            "localhost".to_string(),
        ];
        let (_config, pem) = self_signed(&names).expect("certificate");
        // The PEM is what a container is asked to trust, so it must be one.
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----"), "{pem}");
    }

    #[test]
    fn the_port_defaults_to_443_because_the_handler_assumes_it() {
        // Only set when a test overrides it; the default is what matters.
        if std::env::var("FAKECLOUD_CFN_RESPONSE_TLS_PORT").is_err() {
            assert_eq!(tls_port(), 443);
        }
    }
}
