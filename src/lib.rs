//! # Webnook
//!
//! A small Rust library for fast, independently hosted showcase websites:
//! personal sites, portfolios, startup and product pages, with a few dynamic
//! endpoints. Add Webnook to an application, supply pages and routes, and
//! deploy one server binary.
//!
//! Webnook provides server infrastructure (page and asset serving, Axum route
//! composition, bounded request handling, graceful shutdown, and optional
//! TLS, SQLite, and outbound HTTP) while the site keeps its content, routes,
//! and business logic. It is not a CMS, a site builder, or a full-stack
//! framework.
//!
//! ## Smallest useful application
//!
//! ```no_run
//! use axum::{routing::get, Json};
//! use webnook::Server;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() -> webnook::Result<()> {
//!     Server::new()
//!         .static_dir("./public")
//!         .on("/api/hello", get(|| async { Json("Hello from Webnook!") }))
//!         .bind("127.0.0.1:3000")
//!         .serve()
//!         .await
//! }
//! ```
//!
//! ## Pages and assets
//!
//! Content is served as a fallback behind explicit routes:
//!
//! 1. Routes registered with [`Server::on`] or [`Server::router`] always win,
//!    regardless of builder order. A route that rejects the method answers
//!    `405` and never falls through to files.
//! 2. Embedded [`Assets`] (registered by hand or with [`Assets::embed`]).
//! 3. A [`Server::static_dir`], for development or content updated
//!    independently of the binary.
//! 4. The [`Server::not_found`] page with a real `404`, or an empty `404`.
//!
//! Both content sources resolve paths the same way (`/` and `/blog/` serve
//! `index.html`, `/about` serves `about.html`, `/blog` redirects to `/blog/`),
//! only answer `GET` and `HEAD`, send UTF-8 content types, ETags, and
//! `Cache-Control` from [`Server::cache`] rules, and never reach outside
//! their root. Compressible embedded assets are gzipped once at startup
//! ([`Server::compress`]); static directories serve precompressed `.br` and
//! `.gz` siblings.
//!
//! ## Routes, state, and per-route limits
//!
//! Handlers are ordinary Axum handlers. Supply state with
//! [`axum::Router::with_state`] and merge the router with [`Server::router`].
//! Tighter limits for one route are Axum layers on that route. The
//! server-wide body cap remains an upper bound, even if a route disables
//! [`axum::extract::DefaultBodyLimit`]:
//!
//! ```
//! use std::time::Duration;
//! use axum::{extract::DefaultBodyLimit, http::StatusCode, routing::post};
//! use tower_http::timeout::TimeoutLayer;
//!
//! async fn contact(body: String) -> StatusCode {
//!     StatusCode::ACCEPTED
//! }
//!
//! let server = webnook::Server::new().on(
//!     "/api/contact",
//!     post(contact).layer((
//!         DefaultBodyLimit::max(16 * 1024),
//!         TimeoutLayer::with_status_code(StatusCode::SERVICE_UNAVAILABLE, Duration::from_secs(5)),
//!     )),
//! );
//! ```
//!
//! ## Limits and overload
//!
//! Every server is bounded by [`Limits`]: request body size (`413`),
//! concurrent requests (excess requests wait, then `503` at the request
//! timeout), open connections (excess connections wait in the OS backlog),
//! and the time a client may take to send headers or finish the TLS
//! handshake (slow clients are disconnected). The defaults suit a 1 CPU /
//! 512 MB host; see [`Limits`] for values.
//!
//! ## Client addresses and proxies
//!
//! [`ClientIp`] gives the visitor's address for rate limiting. Behind a
//! reverse proxy or load balancer, declare it with [`Server::trust_proxy`];
//! `X-Forwarded-For` is ignored from any other peer.
//!
//! ## Shutdown
//!
//! [`Server::serve`] runs until SIGTERM or Ctrl-C (or a custom
//! [`Server::shutdown_signal`]). It then stops accepting connections, closes
//! idle keep-alive connections, lets in-flight requests finish within
//! [`Limits::shutdown_grace`], closes the rest, and returns `Ok(())`.
//!
//! ## Errors
//!
//! Configuration and startup problems (missing listener, bind failures,
//! unreadable static roots or certificates, invalid header or proxy values)
//! are returned as [`Error`] from [`Server::build`] and [`Server::serve`].
//! Route registration mistakes panic at the call site, as in Axum. Webnook
//! never terminates the process, and internal errors are never sent to
//! visitors.
//!
//! ## Runtime and logging
//!
//! The application owns the Tokio runtime; Webnook never starts one. On a
//! single-core host, `#[tokio::main(flavor = "current_thread")]` is enough.
//! Webnook emits [`tracing`] events and leaves the subscriber and log
//! destination to the application.
//!
//! ## Optional features
//!
//! None are enabled by default.
//!
//! | Feature | Adds |
//! | --- | --- |
//! | `tls` | [`Server::tls`] and [`Server::https_redirect`] with `rustls` |
//! | `embed` | [`Assets::embed`] for folders embedded with `rust-embed` |
//! | `sqlite` | [`Sqlite`], one SQLite connection on a worker thread |
//! | `http-client` | [`HttpClient`], a bounded outbound `reqwest` client |

#![warn(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]

mod assets;
mod cache;
mod client_ip;
mod conn;
mod error;
mod limits;
mod server;
mod shutdown;
mod static_dir;
mod url;

#[cfg(feature = "http-client")]
mod http_client;
#[cfg(feature = "sqlite")]
mod sqlite;
#[cfg(feature = "tls")]
mod tls;

pub use assets::{Asset, Assets};
pub use cache::Cache;
pub use client_ip::ClientIp;
pub use error::{Error, Result};
pub use limits::Limits;
pub use server::Server;

#[cfg(feature = "tls")]
pub use error::TlsError;
#[cfg(feature = "http-client")]
pub use http_client::{HttpClient, HttpClientOptions, HttpResponse};
#[cfg(feature = "sqlite")]
pub use sqlite::{Sqlite, SqliteOptions};

pub use axum;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
#[cfg(feature = "embed")]
pub use rust_embed;
