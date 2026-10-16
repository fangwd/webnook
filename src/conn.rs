//! Connection accept loop shared by the HTTP, HTTPS, and redirect listeners.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tokio::task::JoinSet;
use tower::ServiceExt;

use crate::limits::Limits;

#[cfg(feature = "tls")]
pub(crate) use tokio_rustls::TlsAcceptor;

/// Uninhabited stand-in so listener code is identical with and without `tls`.
#[cfg(not(feature = "tls"))]
#[derive(Clone)]
pub(crate) enum TlsAcceptor {}

/// Accept connections until `stop` becomes `true`, then let in-flight requests
/// finish within [`Limits::shutdown_grace`] and close whatever remains. New
/// TLS connections use the latest acceptor in `tls`, so certificates can be
/// replaced while serving.
pub(crate) async fn run(
    listener: TcpListener,
    app: Router,
    limits: Limits,
    tls: Option<watch::Receiver<TlsAcceptor>>,
    mut stop: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(limits.max_connections.max(1)));
    let mut http = http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(limits.header_read_timeout);
    let mut conns = JoinSet::new();

    loop {
        let accept = async {
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .expect("connection semaphore is never closed");
            (permit, listener.accept().await)
        };
        let (permit, stream, remote) = tokio::select! {
            _ = stopped(&mut stop) => break,
            Some(_) = conns.join_next() => continue,
            (permit, accepted) = accept => match accepted {
                Ok((stream, remote)) => (permit, stream, remote),
                Err(err) => {
                    accept_error(err).await;
                    continue;
                }
            },
        };

        let conn = Conn {
            app: app.clone(),
            http: http.clone(),
            remote,
            stop: stop.clone(),
        };
        let tls = tls.as_ref().map(|current| current.borrow().clone());
        let handshake_timeout = limits.header_read_timeout;
        conns.spawn(async move {
            let _permit = permit;
            let _ = stream.set_nodelay(true);
            match tls {
                Some(acceptor) => conn.serve_tls(acceptor, stream, handshake_timeout).await,
                None => conn.serve(stream).await,
            }
        });
    }

    drop(listener);
    let drain = async { while conns.join_next().await.is_some() {} };
    if tokio::time::timeout(limits.shutdown_grace, drain)
        .await
        .is_err()
    {
        tracing::warn!(
            remaining = conns.len(),
            "shutdown grace elapsed; closing remaining connections"
        );
        conns.shutdown().await;
    }
}

struct Conn {
    app: Router,
    http: http1::Builder,
    remote: SocketAddr,
    stop: watch::Receiver<bool>,
}

impl Conn {
    async fn serve<I>(mut self, io: I)
    where
        I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let remote = self.remote;
        let service = self.app.map_request(move |mut req: Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo(remote));
            req
        });
        let conn = self
            .http
            .serve_connection(TokioIo::new(io), TowerToHyperService::new(service))
            .with_upgrades();
        tokio::pin!(conn);

        let result = tokio::select! {
            result = conn.as_mut() => result,
            _ = stopped(&mut self.stop) => {
                conn.as_mut().graceful_shutdown();
                conn.await
            }
        };
        if let Err(err) = result {
            tracing::debug!(%remote, %err, "connection closed with error");
        }
    }

    #[cfg(feature = "tls")]
    async fn serve_tls(self, acceptor: TlsAcceptor, stream: TcpStream, timeout: Duration) {
        match tokio::time::timeout(timeout, acceptor.accept(stream)).await {
            Ok(Ok(io)) => self.serve(io).await,
            Ok(Err(err)) => tracing::debug!(remote = %self.remote, %err, "TLS handshake failed"),
            Err(_) => tracing::debug!(remote = %self.remote, "TLS handshake timed out"),
        }
    }

    #[cfg(not(feature = "tls"))]
    async fn serve_tls(self, acceptor: TlsAcceptor, _: TcpStream, _: Duration) {
        match acceptor {}
    }
}

/// Resolve once shutdown has been requested or the server is gone.
pub(crate) async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|stopping| *stopping).await;
}

async fn accept_error(err: std::io::Error) {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};

    // Per-connection failures; the listener itself is fine.
    if matches!(
        err.kind(),
        ConnectionAborted | ConnectionRefused | ConnectionReset
    ) {
        return;
    }
    // Typically file-descriptor exhaustion: back off instead of spinning.
    tracing::error!(%err, "failed to accept connection");
    tokio::time::sleep(Duration::from_secs(1)).await;
}
