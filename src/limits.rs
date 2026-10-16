//! Bounded request-handling configuration.

use std::time::Duration;

/// Limits applied to every connection and request. All fields have
/// conservative defaults suitable for a small showcase site on a modest server.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum accepted request body size, including raw body streams.
    /// Oversized `Content-Length` values are rejected with `413` before the
    /// handler runs. Streaming bodies fail when read past the limit; Axum's
    /// buffered extractors return `413`, and raw body readers must handle
    /// that read error themselves. Route layers cannot raise this cap.
    /// Default: 2 MiB.
    pub max_body_bytes: usize,

    /// Maximum number of requests handled concurrently across the whole
    /// server. Additional requests wait for a free slot; the wait counts
    /// toward [`Limits::request_timeout`]. Minimum 1. Default: 64.
    pub max_concurrency: usize,

    /// Maximum number of open client connections. When reached, new
    /// connections stay in the OS accept backlog until one closes.
    /// Minimum 1. Default: 256.
    pub max_connections: usize,

    /// Time allowed for a client to complete the TLS handshake and to send
    /// each request's headers, including the idle wait for the next request
    /// on a keep-alive connection. Slow clients are disconnected.
    /// Default: 10s.
    pub header_read_timeout: Duration,

    /// Per-request timeout covering the wait for a concurrency slot and the
    /// handler. Requests exceeding it receive `503 Service Unavailable`.
    /// Default: 30s.
    pub request_timeout: Duration,

    /// Deadline for draining in-flight requests during graceful shutdown.
    /// Connections still open afterwards are closed. Default: 10s.
    pub shutdown_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_body_bytes: 2 * 1024 * 1024,
            max_concurrency: 64,
            max_connections: 256,
            header_read_timeout: Duration::from_secs(10),
            request_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(10),
        }
    }
}
