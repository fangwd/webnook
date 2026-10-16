//! Optional outbound HTTP client (the `http-client` feature).
//!
//! One reusable [`reqwest::Client`] with explicit timeouts, a cap on
//! concurrent requests, and a bounded response body size. Create it once at
//! startup and share it through application state; never per request.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use reqwest::header::HeaderMap;
use reqwest::{IntoUrl, Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use tokio::sync::Semaphore;

use crate::error::{Error, Result};

/// Configuration for [`HttpClient`].
#[derive(Debug, Clone)]
pub struct HttpClientOptions {
    /// Connection establishment timeout. Default: 5s.
    pub connect_timeout: Duration,
    /// Total timeout from connecting until the body is read. Default: 15s.
    pub request_timeout: Duration,
    /// Maximum response body size read into memory. Default: 4 MiB.
    pub max_response_bytes: usize,
    /// Maximum concurrent requests; further sends wait for a slot.
    /// Default: 16.
    pub max_in_flight: usize,
}

impl Default for HttpClientOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(15),
            max_response_bytes: 4 * 1024 * 1024,
            max_in_flight: 16,
        }
    }
}

/// A shared outbound HTTP client with bounded resources. Cloning is cheap
/// and shares the connection pool and limits.
///
/// ```no_run
/// # async fn run(client: webnook::HttpClient) -> webnook::Result<()> {
/// let resp = client
///     .send(
///         client
///             .post("https://api.example.com/v1/messages")
///             .bearer_auth("api-key-from-env")
///             .json(&serde_json::json!({ "to": "ada@example.com" })),
///     )
///     .await?
///     .error_for_status()?;
/// let reply: serde_json::Value = resp.json()?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct HttpClient {
    inner: reqwest::Client,
    permits: Arc<Semaphore>,
    max_response_bytes: usize,
}

impl HttpClient {
    /// Build a client from `options`.
    pub fn new(options: HttpClientOptions) -> Result<Self> {
        let inner = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .timeout(options.request_timeout)
            .build()?;
        Ok(Self::with_client(inner, &options))
    }

    /// Wrap an existing client. Its own timeouts and settings apply;
    /// `connect_timeout` and `request_timeout` in `options` are ignored.
    pub fn with_client(client: reqwest::Client, options: &HttpClientOptions) -> Self {
        Self {
            inner: client,
            permits: Arc::new(Semaphore::new(options.max_in_flight.max(1))),
            max_response_bytes: options.max_response_bytes,
        }
    }

    /// Start a `GET` request; finish it with [`HttpClient::send`].
    pub fn get(&self, url: impl IntoUrl) -> RequestBuilder {
        self.inner.get(url)
    }

    /// Start a `POST` request; finish it with [`HttpClient::send`].
    pub fn post(&self, url: impl IntoUrl) -> RequestBuilder {
        self.inner.post(url)
    }

    /// Start a request with any method; finish it with [`HttpClient::send`].
    pub fn request(&self, method: Method, url: impl IntoUrl) -> RequestBuilder {
        self.inner.request(method, url)
    }

    /// Send `request` within the concurrency limit and read its body up to
    /// [`HttpClientOptions::max_response_bytes`]. Non-success statuses are
    /// returned as responses; use [`HttpResponse::error_for_status`] to turn
    /// them into errors.
    pub async fn send(&self, request: RequestBuilder) -> Result<HttpResponse> {
        let request = request.build()?;
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("client semaphore is never closed");
        let resp = self.inner.execute(request).await?;
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = read_bounded(resp, self.max_response_bytes).await?;
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    /// The underlying [`reqwest::Client`]. Requests sent through it bypass
    /// the concurrency and body-size limits.
    pub fn raw(&self) -> &reqwest::Client {
        &self.inner
    }
}

/// A fully read, size-bounded response.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// Response status.
    pub status: StatusCode,
    /// Response headers.
    pub headers: HeaderMap,
    /// Response body.
    pub body: Bytes,
}

impl HttpResponse {
    /// Return [`Error::HttpStatus`] for `4xx` and `5xx` statuses. The body is
    /// dropped so provider error details never reach visitors by accident;
    /// inspect [`HttpResponse::body`] first if you need them.
    pub fn error_for_status(self) -> Result<Self> {
        if self.status.is_client_error() || self.status.is_server_error() {
            Err(Error::HttpStatus(self.status))
        } else {
            Ok(self)
        }
    }

    /// Deserialize the body as JSON.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_slice(&self.body).map_err(Error::HttpDecode)
    }

    /// The body as text, replacing invalid UTF-8.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Read the body chunk by chunk, failing as soon as it exceeds `max` so an
/// oversized or unannounced body never accumulates in memory.
async fn read_bounded(mut resp: reqwest::Response, max: usize) -> Result<Bytes> {
    if resp.content_length().is_some_and(|len| len > max as u64) {
        return Err(Error::HttpBodyTooLarge(max));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() + chunk.len() > max {
            return Err(Error::HttpBodyTooLarge(max));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body.into())
}
