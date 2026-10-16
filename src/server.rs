//! The [`Server`] builder and serving lifecycle.

use std::fmt::Display;
use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Request, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::MethodRouter;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tower::limit::GlobalConcurrencyLimitLayer;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::assets::{Asset, AssetTable, Assets, Entry};
use crate::cache::{Cache, CacheRules};
use crate::client_ip::{self, Cidr};
use crate::conn;
use crate::error::{Error, Result};
use crate::limits::Limits;
use crate::shutdown::shutdown_signal;
use crate::static_dir::StaticDir;

/// State shared by the site fallback service.
#[derive(Clone)]
struct FallbackState {
    assets: Arc<AssetTable>,
    static_dir: Option<StaticDir>,
    not_found: Option<Arc<Entry>>,
}

/// A bind target, resolved when configured and bound when serving.
struct BindAddr {
    display: String,
    addrs: std::io::Result<Vec<SocketAddr>>,
}

impl BindAddr {
    fn new(addr: impl ToSocketAddrs + Display) -> Self {
        Self {
            display: addr.to_string(),
            addrs: addr.to_socket_addrs().map(Iterator::collect),
        }
    }

    async fn bind(self) -> Result<TcpListener> {
        let display = self.display;
        let error = |source| Error::Bind {
            addr: display.clone(),
            source,
        };
        let addrs = self.addrs.map_err(error)?;
        TcpListener::bind(&addrs[..]).await.map_err(error)
    }
}

enum Listen {
    Addr(BindAddr),
    Listener(TcpListener),
}

#[cfg(feature = "tls")]
#[derive(Clone)]
struct TlsFiles {
    cert: PathBuf,
    key: PathBuf,
}

type ShutdownSignal = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Listener parts that exist only with direct HTTPS.
#[derive(Default)]
struct Https {
    acceptor: Option<watch::Receiver<conn::TlsAcceptor>>,
    redirect: Option<(TcpListener, Router)>,
    reload: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

/// A chainable builder for a Webnook site server.
///
/// ```no_run
/// use axum::routing::get;
/// use webnook::Server;
///
/// # async fn run() -> webnook::Result<()> {
/// Server::new()
///     .static_dir("./public")
///     .on("/health", get(|| async { "ok" }))
///     .bind("127.0.0.1:3000")
///     .serve()
///     .await
/// # }
/// ```
///
/// Route registration follows Axum: [`Server::on`] and [`Server::router`]
/// panic at the call site on invalid paths or overlapping method handlers,
/// so such mistakes surface on the first run. Errors that depend on the
/// environment or on values (bind failures, unreadable directories or
/// certificates, invalid header values) are returned from [`Server::build`]
/// and [`Server::serve`].
///
/// Handlers can extract the visitor's address with [`crate::ClientIp`], or the
/// raw socket peer with
/// [`axum::extract::ConnectInfo<SocketAddr>`](axum::extract::ConnectInfo).
///
/// # Response headers
///
/// Every response gets these headers unless the handler set them itself:
///
/// | Header | Default |
/// | --- | --- |
/// | `X-Content-Type-Options` | `nosniff` |
/// | `Referrer-Policy` | `strict-origin-when-cross-origin` |
/// | `X-Frame-Options` | `SAMEORIGIN` |
/// | `Strict-Transport-Security` | `max-age=31536000`, only with [`Server::tls`] |
///
/// Change them with [`Server::header`] and drop them with
/// [`Server::remove_header`].
pub struct Server {
    listen: Option<Listen>,
    router: Router,
    static_dir: Option<PathBuf>,
    assets: Assets,
    not_found: Option<Asset>,
    cache_rules: CacheRules,
    compress: bool,
    limits: Limits,
    headers: Vec<(HeaderName, HeaderValue)>,
    removed_headers: Vec<HeaderName>,
    trusted_proxies: Vec<Cidr>,
    config_error: Option<String>,
    shutdown: Option<ShutdownSignal>,
    #[cfg(feature = "tls")]
    tls: Option<TlsFiles>,
    #[cfg(feature = "tls")]
    https_redirect: Option<(String, BindAddr)>,
}

impl Server {
    /// Create a builder with bounded defaults and no routes or listeners.
    pub fn new() -> Self {
        Self {
            listen: None,
            router: Router::new(),
            static_dir: None,
            assets: Assets::new(),
            not_found: None,
            cache_rules: CacheRules::default(),
            compress: true,
            limits: Limits::default(),
            headers: Vec::new(),
            removed_headers: Vec::new(),
            trusted_proxies: Vec::new(),
            config_error: None,
            shutdown: None,
            #[cfg(feature = "tls")]
            tls: None,
            #[cfg(feature = "tls")]
            https_redirect: None,
        }
    }

    /// Serve a filesystem directory at the site root as a fallback for
    /// unmatched paths. Paths resolve like embedded [`Assets`]: `/` and
    /// `/blog/` serve `index.html`, `/about` serves `about.html`.
    ///
    /// The path is resolved against the working directory when the server is
    /// built. Requests, including those through symlinks, can only reach
    /// files inside that root, and never hidden files or directories (names
    /// starting with `.`, except `.well-known`). Embedded assets take
    /// precedence.
    pub fn static_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.static_dir = Some(path.as_ref().to_path_buf());
        self
    }

    /// Serve embedded pages and assets as the site fallback. Explicit routes
    /// take precedence over these assets.
    pub fn assets(mut self, assets: Assets) -> Self {
        self.assets = assets;
        self
    }

    /// Serve `page` with a `404 Not Found` status for unmatched paths. The
    /// content type defaults to HTML.
    pub fn not_found(mut self, page: impl Into<Asset>) -> Self {
        self.not_found = Some(page.into());
        self
    }

    /// Use `cache` for embedded and static files whose path starts with
    /// `prefix`, e.g. `.cache("/assets/", Cache::Immutable)` for
    /// content-hashed bundles. The longest matching prefix wins; an
    /// [`Asset::cache`] override wins over any rule. Without a rule, HTML is
    /// [`Cache::Revalidate`] and other files are `Cache::MaxAge(60)`.
    pub fn cache(mut self, prefix: &str, cache: Cache) -> Self {
        self.cache_rules.add(prefix, cache);
        self
    }

    /// Whether to gzip compressible embedded assets once at startup and
    /// serve the compressed copy to clients that accept it. Default: `true`.
    /// Static directories serve sibling `.br`/`.gz` files when present.
    pub fn compress(mut self, enabled: bool) -> Self {
        self.compress = enabled;
        self
    }

    /// Register an Axum [`MethodRouter`] at a path. Calling `on` again for
    /// the same path adds methods, e.g. `get(show)` then `post(submit)`.
    ///
    /// # Panics
    ///
    /// If the path is invalid or a method is already handled for it.
    #[track_caller]
    pub fn on(mut self, path: &str, methods: MethodRouter) -> Self {
        self.router = self.router.route(path, methods);
        self
    }

    /// Merge an existing Axum [`Router`] (state already supplied) for advanced
    /// routing and middleware. May be called more than once. The site fallback
    /// replaces any fallback set on `router`.
    ///
    /// # Panics
    ///
    /// If `router` overlaps routes already registered.
    #[track_caller]
    pub fn router(mut self, router: Router) -> Self {
        self.router = self.router.merge(router);
        self
    }

    /// Listen on an address such as `"127.0.0.1:3000"`, `"[::]:443"`, or a
    /// [`SocketAddr`]. Failures are reported by [`Server::serve`].
    pub fn bind(mut self, addr: impl ToSocketAddrs + Display) -> Self {
        self.listen = Some(Listen::Addr(BindAddr::new(addr)));
        self
    }

    /// Serve on an already-bound listener instead of [`Server::bind`], e.g.
    /// one bound to port `0` in tests or inherited from a service manager.
    pub fn listener(mut self, listener: TcpListener) -> Self {
        self.listen = Some(Listen::Listener(listener));
        self
    }

    /// Replace the default shutdown trigger (SIGTERM or Ctrl-C) with `signal`.
    pub fn shutdown_signal(mut self, signal: impl Future<Output = ()> + Send + 'static) -> Self {
        self.shutdown = Some(Box::pin(signal));
        self
    }

    /// Override the default request-handling limits.
    pub fn limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self
    }

    /// Set a header on every response that does not already have it, e.g.
    /// `.header("content-security-policy", "default-src 'self'")`. Replaces a
    /// default with the same name. Invalid names or values are reported by
    /// [`Server::build`].
    pub fn header<K, V>(mut self, name: K, value: V) -> Self
    where
        K: TryInto<HeaderName>,
        K::Error: Display,
        V: TryInto<HeaderValue>,
        V::Error: Display,
    {
        match (name.try_into(), value.try_into()) {
            (Ok(name), Ok(value)) => self.headers.push((name, value)),
            (Err(err), _) => self.record_error(format!("invalid header name: {err}")),
            (_, Err(err)) => self.record_error(format!("invalid header value: {err}")),
        }
        self
    }

    /// Stop adding a default or previously configured header.
    pub fn remove_header<K>(mut self, name: K) -> Self
    where
        K: TryInto<HeaderName>,
        K::Error: Display,
    {
        match name.try_into() {
            Ok(name) => self.removed_headers.push(name),
            Err(err) => self.record_error(format!("invalid header name: {err}")),
        }
        self
    }

    /// Trust `X-Forwarded-For` from proxies in `range` (CIDR notation such as
    /// `"10.0.0.0/8"`, or a single address such as `"127.0.0.1"`) when
    /// resolving [`crate::ClientIp`]. Call once per range. Only trust ranges
    /// that cannot be reached except through your proxy or load balancer.
    pub fn trust_proxy(mut self, range: &str) -> Self {
        match Cidr::parse(range) {
            Some(cidr) => self.trusted_proxies.push(cidr),
            None => self.record_error(format!("invalid proxy range: {range:?}")),
        }
        self
    }

    /// Serve HTTPS on the bound listener with a PEM certificate chain and
    /// private key. Requires the `tls` feature.
    ///
    /// On Unix, sending `SIGHUP` reloads both files without dropping
    /// connections, e.g. from a certbot deploy hook. If the new files are
    /// invalid, the error is logged and the current certificate stays.
    #[cfg(feature = "tls")]
    pub fn tls(mut self, cert: impl AsRef<Path>, key: impl AsRef<Path>) -> Self {
        self.tls = Some(TlsFiles {
            cert: cert.as_ref().to_path_buf(),
            key: key.as_ref().to_path_buf(),
        });
        self
    }

    /// Also listen for plain HTTP on `addr` and `301`-redirect every request
    /// to `https://{canonical_host}` with the same path. Requires
    /// [`Server::tls`].
    #[cfg(feature = "tls")]
    pub fn https_redirect(
        mut self,
        canonical_host: impl Into<String>,
        addr: impl ToSocketAddrs + Display,
    ) -> Self {
        self.https_redirect = Some((canonical_host.into(), BindAddr::new(addr)));
        self
    }

    /// Validate configuration, start serving, and return errors to the caller.
    ///
    /// Runs until the shutdown signal fires (SIGTERM or Ctrl-C by default),
    /// then stops accepting connections, lets in-flight requests finish
    /// within [`Limits::shutdown_grace`], and closes any that remain.
    pub async fn serve(mut self) -> Result<()> {
        let listen = self.listen.take().ok_or(Error::NoBindAddress)?;
        let shutdown = self
            .shutdown
            .take()
            .unwrap_or_else(|| Box::pin(shutdown_signal()));
        let limits = self.limits.clone();

        let (stop_tx, stop_rx) = watch::channel(false);
        let https = self.https(stop_rx.clone()).await?;

        let app = self.build()?;
        let listener = match listen {
            Listen::Addr(addr) => addr.bind().await?,
            Listen::Listener(listener) => listener,
        };
        let local = listener.local_addr()?;
        tracing::info!(%local, https = https.acceptor.is_some(), "webnook listening");

        let site = conn::run(
            listener,
            app,
            limits.clone(),
            https.acceptor,
            stop_rx.clone(),
        );
        let redirect = async move {
            if let Some((listener, app)) = https.redirect {
                conn::run(listener, app, limits, None, stop_rx).await;
            }
        };
        let reload = async move {
            if let Some(reload) = https.reload {
                reload.await;
            }
        };
        let trigger = async move {
            shutdown.await;
            tracing::info!("shutting down");
            let _ = stop_tx.send(true);
        };
        tokio::join!(site, redirect, trigger, reload);
        Ok(())
    }

    /// Build the Axum application without binding a listener. Useful for tests
    /// and for embedding Webnook in a larger Axum app.
    pub fn build(self) -> Result<Router> {
        if let Some(message) = self.config_error {
            return Err(Error::Config(message));
        }
        #[cfg(feature = "tls")]
        let https = self.tls.is_some();
        #[cfg(not(feature = "tls"))]
        let https = false;
        let headers = Arc::new(response_headers(https, self.headers, &self.removed_headers));

        let not_found = self
            .not_found
            .map(|page| {
                Entry::new(
                    page.cache(Cache::NoStore),
                    "404.html",
                    &self.cache_rules,
                    false,
                )
                .map(Arc::new)
            })
            .transpose()?;
        let assets = AssetTable::new(self.assets, &self.cache_rules, self.compress)?;
        let rules = Arc::new(self.cache_rules);
        let state = FallbackState {
            assets: Arc::new(assets),
            static_dir: self
                .static_dir
                .as_deref()
                .map(|dir| StaticDir::new(dir, rules))
                .transpose()?,
            not_found,
        };
        let fallback = Router::new().fallback(fallback_handler).with_state(state);

        let limits = self.limits;
        let trusted = Arc::new(self.trusted_proxies);
        Ok(self
            .router
            .fallback_service(fallback)
            .layer(CatchPanicLayer::new())
            .layer(DefaultBodyLimit::max(limits.max_body_bytes))
            .layer(RequestBodyLimitLayer::new(limits.max_body_bytes))
            .layer(GlobalConcurrencyLimitLayer::new(
                limits.max_concurrency.max(1),
            ))
            .layer(TimeoutLayer::with_status_code(
                StatusCode::SERVICE_UNAVAILABLE,
                limits.request_timeout,
            ))
            .layer(middleware::from_fn(move |req, next| {
                client_ip::resolve(trusted.clone(), req, next)
            }))
            .layer(TraceLayer::new_for_http())
            .layer(middleware::from_fn(move |req, next| {
                set_headers(headers.clone(), req, next)
            })))
    }

    /// Load TLS material, bind the redirect listener, and register the
    /// certificate reload signal before serving starts.
    #[cfg(feature = "tls")]
    async fn https(&mut self, stop: watch::Receiver<bool>) -> Result<Https> {
        let Some(files) = self.tls.clone() else {
            return match self.https_redirect {
                Some(_) => Err(Error::Config("https_redirect requires tls".into())),
                None => Ok(Https::default()),
            };
        };
        let (current, acceptor) = watch::channel(crate::tls::acceptor(&files.cert, &files.key)?);
        let redirect = match self.https_redirect.take() {
            Some((host, addr)) => {
                validate_host(&host)?;
                let listener = addr.bind().await?;
                tracing::info!(addr = %listener.local_addr()?, "http->https redirect listening");
                Some((listener, crate::tls::redirect_app(&host)))
            }
            None => None,
        };
        let reload = crate::tls::reloader(current, files.cert, files.key, stop)?;
        Ok(Https {
            acceptor: Some(acceptor),
            redirect,
            reload: Some(reload),
        })
    }

    #[cfg(not(feature = "tls"))]
    async fn https(&mut self, _stop: watch::Receiver<bool>) -> Result<Https> {
        Ok(Https::default())
    }

    fn record_error(&mut self, message: String) {
        self.config_error.get_or_insert(message);
    }
}

impl Default for Server {
    fn default() -> Self {
        Self::new()
    }
}

fn response_headers(
    https: bool,
    configured: Vec<(HeaderName, HeaderValue)>,
    removed: &[HeaderName],
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    headers.insert(
        header::X_FRAME_OPTIONS,
        HeaderValue::from_static("SAMEORIGIN"),
    );
    if https {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    for (name, value) in configured {
        headers.insert(name, value);
    }
    for name in removed {
        headers.remove(name);
    }
    headers
}

/// Accept only a bare `host` or `host:port` for redirect targets.
#[cfg(feature = "tls")]
fn validate_host(host: &str) -> Result<()> {
    let valid = host
        .parse::<axum::http::uri::Authority>()
        .is_ok_and(|authority| authority.as_str() == host && !host.contains('@'));
    if valid {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "https_redirect host must be a host or host:port, got {host:?}"
        )))
    }
}

async fn set_headers(
    headers: Arc<HeaderMap>,
    req: Request<Body>,
    next: middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    for (name, value) in headers.iter() {
        resp.headers_mut()
            .entry(name.clone())
            .or_insert_with(|| value.clone());
    }
    resp
}

async fn fallback_handler(State(state): State<FallbackState>, req: Request<Body>) -> Response {
    let method = req.method().clone();
    let headers = req.headers().clone();

    if let Some(resp) = state.assets.respond(req.uri(), &method, &headers) {
        return resp;
    }
    if let Some(static_dir) = &state.static_dir {
        if let Some(resp) = static_dir.serve(req).await {
            return resp;
        }
    }
    match &state.not_found {
        Some(page) => page.respond(&method, &headers, StatusCode::NOT_FOUND),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
