//! Typed errors returned to the consuming application.

use std::path::PathBuf;

/// Errors produced while configuring or running a [`crate::Server`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// No bind address was configured.
    #[error(
        "no listener configured; call `.bind(addr)` or `.listener(listener)` before `.serve()`"
    )]
    NoBindAddress,

    /// A static directory root was invalid or inaccessible.
    #[error("static directory root is not accessible: {}", path.display())]
    StaticRoot {
        /// The offending path.
        path: PathBuf,
        /// The underlying I/O error, if any.
        #[source]
        source: Option<std::io::Error>,
    },

    /// A listener could not be bound to the configured address.
    #[error("failed to bind {addr}: {source}")]
    Bind {
        /// The configured address.
        addr: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The builder configuration is inconsistent or contains invalid values.
    #[error("invalid configuration: {0}")]
    Config(String),

    /// A TLS certificate or key could not be loaded.
    #[cfg(feature = "tls")]
    #[error("failed to load TLS material: {0}")]
    Tls(#[from] TlsError),

    /// A SQLite operation failed.
    #[cfg(feature = "sqlite")]
    #[error("sqlite error: {0}")]
    Sqlite(#[source] rusqlite::Error),

    /// The SQLite call queue is full; retry later or shed load.
    #[cfg(feature = "sqlite")]
    #[error("sqlite busy: call queue is full")]
    SqliteBusy,

    /// The SQLite worker thread has stopped.
    #[cfg(feature = "sqlite")]
    #[error("sqlite worker stopped")]
    SqliteClosed,

    /// A SQLite call panicked; its transaction was rolled back.
    #[cfg(feature = "sqlite")]
    #[error("sqlite call panicked")]
    SqlitePanicked,

    /// The database has more migrations applied than the application knows.
    #[cfg(feature = "sqlite")]
    #[error("database schema version {database} is newer than the {known} known migrations")]
    SqliteSchemaAhead {
        /// `PRAGMA user_version` of the database.
        database: usize,
        /// Number of migrations the application supplied.
        known: usize,
    },

    /// An outbound HTTP request failed. The URL in the error has its query,
    /// fragment, and credentials removed so secrets do not reach logs.
    #[cfg(feature = "http-client")]
    #[error("outbound HTTP error: {0}")]
    Http(#[source] reqwest::Error),

    /// An outbound HTTP response had a `4xx` or `5xx` status.
    #[cfg(feature = "http-client")]
    #[error("outbound HTTP request failed with status {0}")]
    HttpStatus(reqwest::StatusCode),

    /// An outbound HTTP response body was not the expected JSON.
    #[cfg(feature = "http-client")]
    #[error("outbound HTTP response is not the expected JSON: {0}")]
    HttpDecode(#[source] serde_json::Error),

    /// An outbound HTTP response body exceeded the configured limit, given
    /// in bytes.
    #[cfg(feature = "http-client")]
    #[error("outbound HTTP response body exceeds the {0}-byte limit")]
    HttpBodyTooLarge(usize),

    /// An I/O error occurred while the server was running.
    #[error("server I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// TLS-specific configuration errors.
#[cfg(feature = "tls")]
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// The certificate or key file could not be read.
    #[error("failed to read {kind} at {}: {source}", path.display())]
    Read {
        /// Human-readable kind of file ("certificate" or "private key").
        kind: &'static str,
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// No valid PEM items were found in the file.
    #[error("no valid PEM {} found in {}", .0, .1.display())]
    Empty(&'static str, PathBuf),

    /// The file is not valid PEM.
    #[error("malformed PEM {kind} in {}: {source}", path.display())]
    Pem {
        /// Human-readable kind of file ("certificate" or "private key").
        kind: &'static str,
        /// The path that failed.
        path: PathBuf,
        /// The underlying PEM error.
        #[source]
        source: rustls::pki_types::pem::Error,
    },

    /// rustls rejected the certificate chain or key.
    #[error("invalid TLS configuration: {0}")]
    Invalid(#[source] rustls::Error),
}

#[cfg(feature = "sqlite")]
impl Error {
    /// Whether this error represents transient saturation rather than failure.
    pub fn is_busy(&self) -> bool {
        matches!(self, Error::SqliteBusy)
    }
}

#[cfg(feature = "http-client")]
impl From<reqwest::Error> for Error {
    fn from(mut err: reqwest::Error) -> Self {
        if let Some(url) = err.url_mut() {
            url.set_query(None);
            url.set_fragment(None);
            let _ = url.set_username("");
            let _ = url.set_password(None);
        }
        Error::Http(err)
    }
}

/// Convenient result alias for Webnook operations.
pub type Result<T> = std::result::Result<T, Error>;
