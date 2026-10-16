//! Embedded page and asset serving.
//!
//! Content registered here lives in the website binary. Only registered assets
//! are served; URL paths are never translated into filesystem access.

use std::borrow::Cow;
use std::collections::HashMap;

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

use crate::cache::{if_none_match, Cache, CacheRules};
use crate::error::{Error, Result};
use crate::url::{
    compressible, content_type, preferred_encodings, redirect_to_directory, SitePath,
};

/// A page or asset body with optional content-type and cache overrides.
///
/// Usually created implicitly from `&'static str`, `&'static [u8]`, `String`,
/// `Vec<u8>`, or [`Bytes`], e.g. from `include_str!` or `include_bytes!`.
#[derive(Debug, Clone)]
pub struct Asset {
    bytes: Bytes,
    content_type: Option<String>,
    cache: Option<Cache>,
}

impl Asset {
    /// Wrap a body. The content type is guessed from the registered path and
    /// the cache policy defaults by content type.
    pub fn new(bytes: impl Into<Bytes>) -> Self {
        Self {
            bytes: bytes.into(),
            content_type: None,
            cache: None,
        }
    }

    /// Override the guessed `Content-Type`.
    pub fn content_type(mut self, content_type: impl Into<String>) -> Self {
        self.content_type = Some(content_type.into());
        self
    }

    /// Override the default cache policy.
    pub fn cache(mut self, cache: Cache) -> Self {
        self.cache = Some(cache);
        self
    }
}

impl From<&'static str> for Asset {
    fn from(text: &'static str) -> Self {
        Self::new(text)
    }
}

impl From<String> for Asset {
    fn from(text: String) -> Self {
        Self::new(text)
    }
}

impl From<&'static [u8]> for Asset {
    fn from(bytes: &'static [u8]) -> Self {
        Self::new(bytes)
    }
}

impl<const N: usize> From<&'static [u8; N]> for Asset {
    fn from(bytes: &'static [u8; N]) -> Self {
        Self::new(&bytes[..])
    }
}

impl From<Vec<u8>> for Asset {
    fn from(bytes: Vec<u8>) -> Self {
        Self::new(bytes)
    }
}

impl From<Bytes> for Asset {
    fn from(bytes: Bytes) -> Self {
        Self::new(bytes)
    }
}

impl From<Cow<'static, [u8]>> for Asset {
    fn from(bytes: Cow<'static, [u8]>) -> Self {
        match bytes {
            Cow::Borrowed(bytes) => Self::new(bytes),
            Cow::Owned(bytes) => Self::new(bytes),
        }
    }
}

/// A collection of embedded pages and assets served as the site fallback.
///
/// Register files by their path, e.g. `/index.html` or `/assets/site.css`.
/// Requests resolve with the same rules as [`crate::Server::static_dir`]:
///
/// | Request | Serves |
/// | --- | --- |
/// | `/` or `/blog/` | `/index.html`, `/blog/index.html` |
/// | `/about` | `/about`, else `/about.html` |
/// | `/blog` (only `/blog/index.html` exists) | `308` redirect to `/blog/` |
///
/// ```
/// use webnook::{Asset, Assets, Cache};
///
/// let assets = Assets::new()
///     .add("/index.html", "<!doctype html><title>Home</title>")
///     .add("/about.html", "<!doctype html><title>About</title>")
///     .add("/app-3f9a1c.js", Asset::new("/* ... */").cache(Cache::Immutable));
/// ```
#[derive(Debug, Clone, Default)]
pub struct Assets {
    files: Vec<(String, Asset)>,
}

impl Assets {
    /// Create an empty collection.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a page or asset at a file path. A later registration for the
    /// same path replaces the earlier one.
    ///
    /// # Panics
    ///
    /// If `path` is not a file path such as `/index.html`.
    #[track_caller]
    pub fn add(mut self, path: &str, asset: impl Into<Asset>) -> Self {
        let key = match SitePath::parse(path) {
            Some(parsed) if !parsed.is_dir => parsed.key(),
            _ => panic!("asset path must name a file, e.g. \"/index.html\": {path:?}"),
        };
        self.files.push((key, asset.into()));
        self
    }

    /// Register every file of a [`rust_embed::RustEmbed`] folder under the
    /// site root. Requires the `embed` feature.
    ///
    /// In debug builds `rust-embed` reads the folder from disk, so content
    /// edits only need a restart; release builds embed the files.
    ///
    /// ```ignore
    /// #[derive(rust_embed::RustEmbed)]
    /// #[folder = "public/"]
    /// struct Site;
    ///
    /// let assets = webnook::Assets::new().embed::<Site>();
    /// ```
    #[cfg(feature = "embed")]
    pub fn embed<E: rust_embed::RustEmbed>(mut self) -> Self {
        for name in E::iter() {
            if let Some(file) = E::get(&name) {
                self.files.push((name.into_owned(), file.data.into()));
            }
        }
        self
    }
}

/// A registered body with every response header precomputed.
#[derive(Debug)]
pub(crate) struct Entry {
    identity: Variant,
    gzip: Option<Variant>,
    content_type: HeaderValue,
    cache_control: HeaderValue,
}

#[derive(Debug)]
struct Variant {
    bytes: Bytes,
    etag: HeaderValue,
}

impl Entry {
    /// Finalize `asset` registered at `path` (without a leading slash),
    /// precompressing it with gzip when `compress` is set and it pays off.
    pub(crate) fn new(
        asset: Asset,
        path: &str,
        rules: &CacheRules,
        compress: bool,
    ) -> Result<Self> {
        let content_type = match asset.content_type {
            Some(value) => HeaderValue::from_str(&value).map_err(|_| {
                Error::Config(format!("invalid content type for {path}: {value:?}"))
            })?,
            None => HeaderValue::from_str(content_type(path).as_ref())
                .expect("guessed MIME types are valid header values"),
        };
        let content_type_str = content_type.to_str().unwrap_or("");
        let cache = asset
            .cache
            .unwrap_or_else(|| rules.for_path(&format!("/{path}"), content_type_str));

        let tag = compute_tag(&asset.bytes);
        let gzip = (compress && compressible(content_type_str))
            .then(|| gzip(&asset.bytes))
            .flatten()
            .map(|bytes| Variant {
                bytes,
                etag: etag(&format!("{tag}-gz")),
            });
        Ok(Self {
            identity: Variant {
                bytes: asset.bytes,
                etag: etag(&tag),
            },
            gzip,
            content_type,
            cache_control: cache.header_value(),
        })
    }

    /// Respond to `GET`/`HEAD` with the best encoding the client accepts,
    /// honoring `If-None-Match` for successful responses.
    pub(crate) fn respond(
        &self,
        method: &Method,
        headers: &HeaderMap,
        status: StatusCode,
    ) -> Response {
        let preferred = preferred_encodings(headers);
        let (variant, gzipped) = match &self.gzip {
            Some(gzip) if accepts_gzip(&preferred) => (gzip, true),
            _ if preferred
                .iter()
                .any(|(encoding, _)| *encoding == "identity") =>
            {
                (&self.identity, false)
            }
            _ => {
                return (
                    StatusCode::NOT_ACCEPTABLE,
                    [(header::VARY, "accept-encoding")],
                )
                    .into_response();
            }
        };

        let mut resp = if status == StatusCode::OK && if_none_match(headers, &variant.etag) {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            let body = if method == Method::HEAD {
                Body::empty()
            } else {
                Body::from(variant.bytes.clone())
            };
            let mut resp = (status, body).into_response();
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, self.content_type.clone());
            h.insert(
                header::CONTENT_LENGTH,
                HeaderValue::from(variant.bytes.len()),
            );
            if gzipped {
                h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            }
            resp
        };

        let h = resp.headers_mut();
        h.insert(header::CACHE_CONTROL, self.cache_control.clone());
        if status == StatusCode::OK {
            h.insert(header::ETAG, variant.etag.clone());
        }
        if self.gzip.is_some() {
            h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
        }
        resp
    }
}

/// The finalized, immutable asset map used while serving.
#[derive(Debug, Default)]
pub(crate) struct AssetTable {
    map: HashMap<String, Entry>,
}

impl AssetTable {
    pub(crate) fn new(assets: Assets, rules: &CacheRules, compress: bool) -> Result<Self> {
        let mut map = HashMap::with_capacity(assets.files.len());
        for (key, asset) in assets.files {
            let entry = Entry::new(asset, &key, rules, compress)?;
            map.insert(key, entry);
        }
        Ok(Self { map })
    }

    /// Respond to a request for a registered asset, or `None` if no asset
    /// matches the path.
    pub(crate) fn respond(
        &self,
        uri: &Uri,
        method: &Method,
        headers: &HeaderMap,
    ) -> Option<Response> {
        let path = SitePath::parse(uri.path())?;
        let entry = match self.lookup(&path) {
            Some(Lookup::File(entry)) => entry,
            Some(Lookup::Directory) => return Some(redirect_to_directory(uri)),
            None => return None,
        };
        if method != Method::GET && method != Method::HEAD {
            return Some(
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    [
                        (header::ALLOW, HeaderValue::from_static("GET, HEAD")),
                        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
                    ],
                )
                    .into_response(),
            );
        }
        Some(entry.respond(method, headers, StatusCode::OK))
    }

    fn lookup(&self, path: &SitePath) -> Option<Lookup<'_>> {
        let key = path.key();
        let index = if key.is_empty() {
            "index.html".to_string()
        } else {
            format!("{key}/index.html")
        };
        if path.is_dir {
            return self.map.get(&index).map(Lookup::File);
        }
        if let Some(entry) = self
            .map
            .get(&key)
            .or_else(|| self.map.get(&format!("{key}.html")))
        {
            return Some(Lookup::File(entry));
        }
        self.map.contains_key(&index).then_some(Lookup::Directory)
    }
}

enum Lookup<'a> {
    File(&'a Entry),
    Directory,
}

/// Whether gzip is among the usable preferences, before an identity fallback.
fn accepts_gzip(preferred: &[(&str, &str)]) -> bool {
    preferred.iter().any(|(encoding, _)| *encoding == "gzip")
}

/// Gzip `bytes`, keeping the result only if it saves at least 10%.
fn gzip(bytes: &[u8]) -> Option<Bytes> {
    use std::io::Write;

    if bytes.len() < 256 {
        return None;
    }
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
    encoder.write_all(bytes).ok()?;
    let compressed = encoder.finish().ok()?;
    (compressed.len() * 10 < bytes.len() * 9).then(|| Bytes::from(compressed))
}

fn etag(tag: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("\"{tag}\"")).expect("hex digits are a valid header value")
}

fn compute_tag(bytes: &[u8]) -> String {
    // FNV-1a 64-bit combined with the length: a cheap, stable validator.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}-{:x}", bytes.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gzip_negotiation() {
        let mut headers = HeaderMap::new();
        assert!(!accepts_gzip(&preferred_encodings(&headers)));
        for (value, expected) in [
            ("gzip, deflate, br", true),
            ("br;q=1.0, gzip;q=0.8", true),
            ("gzip;q=0", false),
            ("*", true),
            ("gzip;q=0, *", false),
            ("identity", false),
        ] {
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
            assert_eq!(
                accepts_gzip(&preferred_encodings(&headers)),
                expected,
                "{value}"
            );
        }
    }

    #[test]
    fn only_worthwhile_gzip_is_kept() {
        assert!(gzip(&[b'a'; 4096]).is_some());
        assert!(gzip(b"tiny").is_none());
    }

    #[test]
    #[should_panic(expected = "asset path must name a file")]
    fn directory_asset_paths_panic() {
        let _ = Assets::new().add("/", "home");
    }
}
