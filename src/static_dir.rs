//! Filesystem serving for [`crate::Server::static_dir`], confined to its root.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderValue, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::cache::{if_none_match, CacheRules};
use crate::error::{Error, Result};
use crate::url::{content_type, preferred_encodings, redirect_to_directory, SitePath};

/// A directory root resolved once at startup. Every request path is resolved
/// through symlinks and must stay inside the root.
#[derive(Clone)]
pub(crate) struct StaticDir {
    root: Arc<PathBuf>,
    rules: Arc<CacheRules>,
}

impl StaticDir {
    pub(crate) fn new(path: &Path, rules: Arc<CacheRules>) -> Result<Self> {
        let root = path.canonicalize().map_err(|source| Error::StaticRoot {
            path: path.to_path_buf(),
            source: Some(source),
        })?;
        if !root.is_dir() {
            return Err(Error::StaticRoot {
                path: path.to_path_buf(),
                source: None,
            });
        }
        Ok(Self {
            root: Arc::new(root),
            rules,
        })
    }

    /// Serve the file for `req`, or `None` when nothing inside the root
    /// matches. Paths resolve like embedded assets: see [`crate::Assets`].
    pub(crate) async fn serve(&self, mut req: Request<Body>) -> Option<Response> {
        let path = SitePath::parse(req.uri().path())?;
        let base: PathBuf = self.root.join(path.segments.iter().collect::<PathBuf>());

        let (file, meta) = if path.is_dir {
            self.file(base.join("index.html")).await?
        } else if let Some(found) = self.file(base.clone()).await {
            found
        } else if let Some(found) = self.file(with_html_extension(&base)).await {
            found
        } else if self.directory(base).await {
            return Some(redirect_to_directory(req.uri()));
        } else {
            return None;
        };

        let mime = content_type(&file.to_string_lossy());
        let cache = self
            .rules
            .for_path(&self.site_path(&file), mime.as_ref())
            .header_value();
        let method = req.method().clone();
        if method != Method::GET && method != Method::HEAD {
            return Some(
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    [
                        (header::ALLOW, "GET, HEAD"),
                        (header::CACHE_CONTROL, "no-store"),
                    ],
                )
                    .into_response(),
            );
        }

        // Resolve each negotiated sibling through the same confinement checks
        // as the original. ServeFile receives only the chosen canonical path,
        // with automatic sibling lookup disabled.
        let mut selected = None;
        for (encoding, suffix) in preferred_encodings(req.headers()) {
            if suffix.is_empty() {
                selected = Some((file.clone(), meta, None));
                break;
            }
            let mut sibling = file.as_os_str().to_owned();
            sibling.push(suffix);
            if let Some((path, meta)) = self.file(PathBuf::from(sibling)).await {
                selected = Some((path, meta, Some(encoding)));
                break;
            }
        }
        let Some((file, meta, encoding)) = selected else {
            return Some(
                (
                    StatusCode::NOT_ACCEPTABLE,
                    [(header::VARY, "accept-encoding")],
                )
                    .into_response(),
            );
        };
        let etag = weak_etag(&meta);
        let not_modified = etag
            .as_ref()
            .is_some_and(|etag| if_none_match(req.headers(), etag));

        // If-None-Match takes precedence even when its value does not match.
        // Otherwise ServeFile could return 304 based solely on a preserved
        // modification time after content (and its ETag) changed.
        if req.headers().contains_key(header::IF_NONE_MATCH) {
            req.headers_mut().remove(header::IF_MODIFIED_SINCE);
        }
        let mut resp = if not_modified {
            StatusCode::NOT_MODIFIED.into_response()
        } else {
            match ServeFile::new_with_mime(&file, &mime).oneshot(req).await {
                Ok(resp) => resp.map(Body::new),
                Err(never) => match never {},
            }
        };
        match resp.status() {
            StatusCode::NOT_FOUND => return None,
            StatusCode::OK | StatusCode::PARTIAL_CONTENT | StatusCode::NOT_MODIFIED => {
                let h = resp.headers_mut();
                h.insert(header::CACHE_CONTROL, cache);
                h.extend(etag.map(|etag| (header::ETAG, etag)));
                h.insert(header::VARY, HeaderValue::from_static("accept-encoding"));
                h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                if let Ok(modified) = meta.modified() {
                    let last_modified = httpdate::fmt_http_date(modified);
                    h.insert(
                        header::LAST_MODIFIED,
                        HeaderValue::from_str(&last_modified)
                            .expect("HTTP dates are valid header values"),
                    );
                }
                if let Some(encoding) = encoding {
                    h.insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
                }
            }
            _ => {}
        }
        Some(resp)
    }

    /// The canonical path and metadata of a regular file inside the root.
    async fn file(&self, path: PathBuf) -> Option<(PathBuf, std::fs::Metadata)> {
        let resolved = self.confine(path).await?;
        let meta = tokio::fs::metadata(&resolved).await.ok()?;
        meta.is_file().then_some((resolved, meta))
    }

    /// The URL-style path of a file below the root, for cache rules.
    fn site_path(&self, file: &Path) -> String {
        let relative = file.strip_prefix(&*self.root).unwrap_or(file);
        let segments: Vec<_> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        format!("/{}", segments.join("/"))
    }

    async fn directory(&self, path: PathBuf) -> bool {
        match self.confine(path).await {
            Some(resolved) => tokio::fs::metadata(resolved)
                .await
                .is_ok_and(|meta| meta.is_dir()),
            None => false,
        }
    }

    /// Resolve symlinks and reject anything that ends up outside the root or
    /// inside a hidden file or directory such as `.git` or `.env`
    /// (`.well-known` stays reachable).
    async fn confine(&self, path: PathBuf) -> Option<PathBuf> {
        let resolved = tokio::fs::canonicalize(path).await.ok()?;
        let relative = resolved.strip_prefix(&*self.root).ok()?;
        let hidden = relative.components().any(|c| {
            let name = c.as_os_str().to_string_lossy();
            name.starts_with('.') && name != ".well-known"
        });
        (!hidden).then_some(resolved)
    }
}

/// A weak validator from size and modification time.
fn weak_etag(meta: &std::fs::Metadata) -> Option<HeaderValue> {
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    HeaderValue::from_str(&format!("W/\"{:x}-{:x}\"", meta.len(), modified.as_nanos())).ok()
}

fn with_html_extension(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".html");
    PathBuf::from(name)
}
