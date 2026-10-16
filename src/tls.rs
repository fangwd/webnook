//! Direct HTTPS serving via `rustls` (the `tls` feature).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::Request;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use rustls::pki_types::pem::{self, PemObject};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::error::{Error, Result, TlsError};

/// Load the certificate chain and private key into a TLS acceptor.
pub(crate) fn acceptor(cert: &Path, key: &Path) -> Result<TlsAcceptor> {
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|items| items.collect::<std::result::Result<Vec<_>, _>>())
        .map_err(|err| pem_error("certificate", cert, err))?;
    if certs.is_empty() {
        return Err(TlsError::Empty("certificate", cert.to_path_buf()).into());
    }
    let key =
        PrivateKeyDer::from_pem_file(key).map_err(|err| pem_error("private key", key, err))?;

    // An explicit provider keeps startup independent of which rustls crypto
    // features other crates in the application enable.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .and_then(|builder| builder.with_no_client_auth().with_single_cert(certs, key))
        .map_err(TlsError::Invalid)?;
    server.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(server)))
}

fn pem_error(kind: &'static str, path: &Path, err: pem::Error) -> Error {
    let path = path.to_path_buf();
    match err {
        pem::Error::Io(source) => TlsError::Read { kind, path, source },
        pem::Error::NoItemsFound => TlsError::Empty(kind, path),
        source => TlsError::Pem { kind, path, source },
    }
    .into()
}

/// A task that reloads the certificate and key on every SIGHUP until `stop`
/// fires. A failed reload is logged and the current certificate stays. The
/// signal handler is registered immediately, so an early SIGHUP cannot
/// terminate the process.
#[cfg(unix)]
pub(crate) fn reloader(
    current: watch::Sender<TlsAcceptor>,
    cert: PathBuf,
    key: PathBuf,
    mut stop: watch::Receiver<bool>,
) -> Result<Pin<Box<dyn Future<Output = ()> + Send>>> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut hangup = signal(SignalKind::hangup())?;
    Ok(Box::pin(async move {
        loop {
            tokio::select! {
                _ = crate::conn::stopped(&mut stop) => return,
                received = hangup.recv() => {
                    if received.is_none() {
                        return;
                    }
                    match acceptor(&cert, &key) {
                        Ok(next) => {
                            current.send_replace(next);
                            tracing::info!("reloaded TLS certificate");
                        }
                        Err(err) => tracing::error!(
                            %err,
                            "TLS reload failed; keeping the current certificate"
                        ),
                    }
                }
            }
        }
    }))
}

/// Certificate reload needs SIGHUP; elsewhere restart the process instead.
#[cfg(not(unix))]
pub(crate) fn reloader(
    _current: watch::Sender<TlsAcceptor>,
    _cert: PathBuf,
    _key: PathBuf,
    _stop: watch::Receiver<bool>,
) -> Result<Pin<Box<dyn Future<Output = ()> + Send>>> {
    Ok(Box::pin(async {}))
}

/// An app that `301`-redirects every request to the same path on
/// `https://{host}`.
pub(crate) fn redirect_app(host: &str) -> Router {
    let host: Arc<str> = Arc::from(host);
    Router::new().fallback(move |req: Request| {
        let host = host.clone();
        async move { redirect_to_https(&host, req) }
    })
}

fn redirect_to_https(host: &str, req: Request) -> Response {
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    match HeaderValue::from_str(&format!("https://{host}{path}")) {
        Ok(location) => (
            StatusCode::MOVED_PERMANENTLY,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => StatusCode::BAD_REQUEST.into_response(),
    }
}
