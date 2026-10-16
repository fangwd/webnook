//! Request-path handling shared by embedded assets and the static directory.

use std::path::{Component, Path};

use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use mime_guess::mime::{self, Mime};

/// A decoded request path split into safe segments.
pub(crate) struct SitePath {
    /// Normal path segments: no empty, `.`, or `..` segments.
    pub(crate) segments: Vec<String>,
    /// Whether the request named a directory (`/` or a trailing slash).
    pub(crate) is_dir: bool,
}

impl SitePath {
    /// Parse a URL path, returning `None` for traversal attempts or segments
    /// that are not plain file names on this platform.
    pub(crate) fn parse(url_path: &str) -> Option<Self> {
        let decoded = percent_decode(url_path);
        let mut segments = Vec::new();
        for segment in decoded.split('/') {
            if segment.is_empty() || segment == "." {
                continue;
            }
            let mut components = Path::new(segment).components();
            match (components.next(), components.next()) {
                (Some(Component::Normal(_)), None) => segments.push(segment.to_string()),
                _ => return None,
            }
        }
        let is_dir = segments.is_empty() || decoded.ends_with('/');
        Some(Self { segments, is_dir })
    }

    /// The path as a `/`-joined key without a leading slash.
    pub(crate) fn key(&self) -> String {
        self.segments.join("/")
    }
}

/// `308` redirect from a directory path without a trailing slash to one with
/// it, preserving the query string.
pub(crate) fn redirect_to_directory(uri: &Uri) -> Response {
    // Collapse leading slashes so `//host` cannot become an off-site redirect.
    let mut location = format!("/{}/", uri.path().trim_matches('/'));
    if let Some(query) = uri.query() {
        location.push('?');
        location.push_str(query);
    }
    match HeaderValue::from_str(&location) {
        Ok(location) => (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Guess a content type from a file name, declaring UTF-8 for text formats.
pub(crate) fn content_type(path: &str) -> Mime {
    let guessed = mime_guess::from_path(path).first_or_octet_stream();
    let textual = guessed.type_() == mime::TEXT
        || matches!(
            guessed.essence_str(),
            "application/javascript" | "application/json" | "application/xml"
        );
    if textual && guessed.get_param(mime::CHARSET).is_none() {
        if let Ok(with_charset) = format!("{guessed}; charset=utf-8").parse() {
            return with_charset;
        }
    }
    guessed
}

/// Whether a content type benefits from gzip or brotli.
pub(crate) fn compressible(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or("").trim();
    essence.starts_with("text/")
        || matches!(
            essence,
            "application/javascript"
                | "application/json"
                | "application/manifest+json"
                | "application/xml"
                | "application/xhtml+xml"
                | "application/rss+xml"
                | "application/atom+xml"
                | "image/svg+xml"
        )
}

/// Usable encodings in preference order, with Brotli winning ties over gzip.
/// Explicit codings override `*`; the last valid duplicate wins. Malformed
/// items are ignored. Implicit identity is a last resort, and choices below
/// identity are omitted because the uncompressed representation is available.
pub(crate) fn preferred_encodings(headers: &HeaderMap) -> Vec<(&'static str, &'static str)> {
    let mut qualities = [None; 3];
    let mut wildcard = None;
    for item in headers
        .get_all(header::ACCEPT_ENCODING)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
    {
        let mut parts = item.split(';');
        let name = parts.next().unwrap_or("").trim();
        let quality = match parts.next() {
            None => 1000,
            Some(param) => {
                let Some((key, value)) = param.trim().split_once('=') else {
                    continue;
                };
                if !key.trim().eq_ignore_ascii_case("q") || parts.next().is_some() {
                    continue;
                }
                let Some(quality) = encoding_quality(value.trim()) else {
                    continue;
                };
                quality
            }
        };
        if name.eq_ignore_ascii_case("br") {
            qualities[0] = Some(quality);
        } else if name.eq_ignore_ascii_case("gzip") || name.eq_ignore_ascii_case("x-gzip") {
            qualities[1] = Some(quality);
        } else if name.eq_ignore_ascii_case("identity") {
            qualities[2] = Some(quality);
        } else if name == "*" {
            wildcard = Some(quality);
        }
    }
    let mut variants = Vec::new();
    for (index, (encoding, suffix)) in [("br", ".br"), ("gzip", ".gz")].into_iter().enumerate() {
        let quality = qualities[index].or(wildcard).unwrap_or(0);
        if quality > 0 {
            variants.push((encoding, suffix, quality));
        }
    }
    let identity_quality = match qualities[2] {
        Some(0) => None,                     // Explicitly forbidden.
        Some(quality) => Some(quality),      // Explicit preference.
        None if wildcard == Some(0) => None, // Excluded by wildcard.
        None => Some(0),                     // Implicit fallback ranks last.
    };
    if let Some(quality) = identity_quality {
        variants.push(("identity", "", quality));
    }
    variants.sort_by_key(|variant| std::cmp::Reverse(variant.2));
    if let Some(index) = variants
        .iter()
        .position(|(encoding, _, _)| *encoding == "identity")
    {
        variants.truncate(index + 1);
    }
    variants
        .into_iter()
        .map(|(encoding, suffix, _)| (encoding, suffix))
        .collect()
}

/// Parse a q-value as thousandths: 0 or 1, optionally followed by up to
/// three fractional digits. Reject floats outside the HTTP q-value grammar.
fn encoding_quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let whole = match whole {
        "0" => 0,
        "1" => 1000,
        _ => return None,
    };
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut quality = whole;
    for (digit, factor) in fraction.bytes().zip([100, 10, 1]) {
        quality += u16::from(digit - b'0') * factor;
    }
    (quality <= 1000).then_some(quality)
}

pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encodings(headers: &HeaderMap) -> Vec<&'static str> {
        preferred_encodings(headers)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    #[test]
    fn encoding_preferences_and_identity_fallback() {
        assert_eq!(encodings(&HeaderMap::new()), ["identity"]);
        for (value, expected) in [
            ("", vec!["identity"]),
            ("gzip, br", vec!["br", "gzip", "identity"]),
            ("X-GZIP", vec!["gzip", "identity"]),
            ("br;q=0.1, gzip;q=0.9", vec!["gzip", "br", "identity"]),
            ("identity;q=1, gzip;q=0.5", vec!["identity"]),
            ("identity;q=0.5, gzip;q=1", vec!["gzip", "identity"]),
            ("gzip;q=0, *", vec!["br", "identity"]),
            ("*;q=0", vec![]),
            ("*;q=0, identity;q=1", vec!["identity"]),
            ("*;q=0, gzip;q=0.001", vec!["gzip"]),
            ("gzip;q=1.000, identity;q=0", vec!["gzip"]),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
            assert_eq!(encodings(&headers), expected, "{value}");
        }
    }

    #[test]
    fn malformed_encoding_qualities_are_ignored() {
        for value in [
            "gzip;q=abc",
            "gzip;q=1.5",
            "gzip;q=-0.1",
            "gzip;q=NaN",
            "gzip;q=inf",
            "gzip;q=1e0",
            "gzip;q=.5",
            "gzip;q=0.0001",
            "gzip;q=1.001",
            "gzip;q=",
            "gzip;q=01",
            "gzip;q=1;q=0",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
            assert_eq!(encodings(&headers), ["identity"], "{value}");
        }
    }

    #[test]
    fn last_valid_encoding_duplicate_wins_across_header_lines() {
        for (value, expected) in [
            ("gzip;q=1, gzip;q=0", vec!["identity"]),
            ("gzip;q=0, gzip;q=1", vec!["gzip", "identity"]),
            ("gzip;q=1, x-gzip;q=0", vec!["identity"]),
            ("gzip;q=1, gzip;q=abc", vec!["gzip", "identity"]),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ACCEPT_ENCODING, HeaderValue::from_static(value));
            assert_eq!(encodings(&headers), expected, "{value}");
        }
        let mut headers = HeaderMap::new();
        headers.append(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0"),
        );
        headers.append(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip;q=0.5, br;q=0.1"),
        );
        assert_eq!(encodings(&headers), ["gzip", "br", "identity"]);
    }

    #[test]
    fn non_utf8_encoding_headers_are_ignored() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::ACCEPT_ENCODING,
            HeaderValue::from_bytes(b"gzip,\xff").unwrap(),
        );
        assert_eq!(encodings(&headers), ["identity"]);
        headers.append(header::ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        assert_eq!(encodings(&headers), ["gzip", "identity"]);
    }

    #[test]
    fn parses_segments_and_directories() {
        let path = SitePath::parse("/a/./b//c").unwrap();
        assert_eq!(path.key(), "a/b/c");
        assert!(!path.is_dir);
        assert!(SitePath::parse("/").unwrap().is_dir);
        assert!(SitePath::parse("/docs/").unwrap().is_dir);
    }

    #[test]
    fn rejects_traversal() {
        assert!(SitePath::parse("/a/../b").is_none());
        assert!(SitePath::parse("/%2e%2e/etc/passwd").is_none());
        assert!(SitePath::parse("/..").is_none());
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("100%"), "100%");
    }

    #[test]
    fn text_types_declare_utf8() {
        assert_eq!(
            content_type("index.html").as_ref(),
            "text/html; charset=utf-8"
        );
        assert_eq!(content_type("site.css").as_ref(), "text/css; charset=utf-8");
        assert_eq!(content_type("logo.png").as_ref(), "image/png");
    }
}
