//! HTTP caching policy shared by embedded assets and the static directory.

use axum::http::{header, HeaderMap, HeaderValue};

/// Cache-Control policy for a page or asset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cache {
    /// `no-cache`: store but revalidate with the ETag on every use. The
    /// default for HTML.
    Revalidate,
    /// `public, max-age=seconds`. The default for other files is one minute.
    MaxAge(u32),
    /// `public, max-age=31536000, immutable`. For content-hashed file names.
    Immutable,
    /// `no-store`: never cache.
    NoStore,
}

impl Cache {
    pub(crate) fn header_value(self) -> HeaderValue {
        match self {
            Cache::Revalidate => HeaderValue::from_static("no-cache"),
            Cache::MaxAge(secs) => HeaderValue::from_str(&format!("public, max-age={secs}"))
                .expect("digits are a valid header value"),
            Cache::Immutable => HeaderValue::from_static("public, max-age=31536000, immutable"),
            Cache::NoStore => HeaderValue::from_static("no-store"),
        }
    }

    fn default_for(content_type: &str) -> Cache {
        if content_type.starts_with("text/html") || content_type == "application/xhtml+xml" {
            Cache::Revalidate
        } else {
            Cache::MaxAge(60)
        }
    }
}

/// Site-wide cache rules keyed by path prefix; the longest match wins.
#[derive(Debug, Clone, Default)]
pub(crate) struct CacheRules {
    rules: Vec<(String, Cache)>,
}

impl CacheRules {
    pub(crate) fn add(&mut self, prefix: &str, cache: Cache) {
        let prefix = format!("/{}", prefix.trim_start_matches('/'));
        self.rules.push((prefix, cache));
    }

    /// The policy for a file at `path` (with a leading `/`).
    pub(crate) fn for_path(&self, path: &str, content_type: &str) -> Cache {
        self.rules
            .iter()
            .filter(|(prefix, _)| path.starts_with(prefix.as_str()))
            .max_by_key(|(prefix, _)| prefix.len())
            .map(|(_, cache)| *cache)
            .unwrap_or_else(|| Cache::default_for(content_type))
    }
}

/// `If-None-Match` using the weak comparison required by RFC 9110.
pub(crate) fn if_none_match(headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let ours = opaque_tag(etag.to_str().unwrap_or(""));
    value.trim() == "*" || value.split(',').any(|tag| opaque_tag(tag) == ours)
}

fn opaque_tag(tag: &str) -> &str {
    let tag = tag.trim();
    tag.strip_prefix("W/").unwrap_or(tag)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins() {
        let mut rules = CacheRules::default();
        rules.add("/assets/", Cache::MaxAge(600));
        rules.add("assets/hashed/", Cache::Immutable);
        assert_eq!(
            rules.for_path("/assets/a.css", "text/css"),
            Cache::MaxAge(600)
        );
        assert_eq!(
            rules.for_path("/assets/hashed/a.css", "text/css"),
            Cache::Immutable
        );
        assert_eq!(
            rules.for_path("/index.html", "text/html"),
            Cache::Revalidate
        );
        assert_eq!(rules.for_path("/logo.png", "image/png"), Cache::MaxAge(60));
    }

    #[test]
    fn weak_etags_match() {
        let etag = HeaderValue::from_static("\"abc\"");
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("W/\"abc\""));
        assert!(if_none_match(&headers, &etag));
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"x\", \"abc\""),
        );
        assert!(if_none_match(&headers, &etag));
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"x\""));
        assert!(!if_none_match(&headers, &etag));
        let weak = HeaderValue::from_static("W/\"abc\"");
        headers.insert(header::IF_NONE_MATCH, HeaderValue::from_static("\"abc\""));
        assert!(if_none_match(&headers, &weak));
    }
}
