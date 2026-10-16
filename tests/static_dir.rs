//! Static directory serving: root confinement, symlinks, and directories.
#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::Path;

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::Router;
use tempfile::TempDir;
use tower::ServiceExt;
use webnook::{Cache, Server};

/// Layout:
///   outside/secret.txt
///   site/index.html, site/about.html, site/site.css, site/docs/index.html
///   site/inside-link.txt  -> site/index.html
///   site/escape.txt       -> outside/secret.txt
///   site/escape-dir       -> outside/
fn fixture() -> (TempDir, Router) {
    let tmp = TempDir::new().unwrap();
    let outside = tmp.path().join("outside");
    let site = tmp.path().join("site");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(site.join("docs")).unwrap();
    std::fs::write(outside.join("secret.txt"), "secret").unwrap();
    std::fs::write(site.join("index.html"), "home").unwrap();
    std::fs::write(site.join("docs/index.html"), "docs").unwrap();
    std::fs::write(site.join("about.html"), "about").unwrap();
    std::fs::write(site.join("site.css"), "body {}").unwrap();
    std::fs::create_dir_all(site.join("evil.com")).unwrap();
    symlink(site.join("index.html"), site.join("inside-link.txt")).unwrap();
    symlink(outside.join("secret.txt"), site.join("escape.txt")).unwrap();
    symlink(&outside, site.join("escape-dir")).unwrap();

    let app = Server::new().static_dir(&site).build().unwrap();
    (tmp, app)
}

async fn send(app: &Router, method: Method, uri: &str) -> axum::response::Response {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    app.clone().oneshot(req).await.unwrap()
}

async fn body(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

#[tokio::test]
async fn serves_files_and_directory_indexes() {
    let (_tmp, app) = fixture();

    let resp = send(&app, Method::GET, "/").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body(resp).await, "home");

    let resp = send(&app, Method::GET, "/docs/").await;
    assert_eq!(body(resp).await, "docs");

    let resp = send(&app, Method::GET, "/about").await;
    assert_eq!(body(resp).await, "about");

    let resp = send(&app, Method::GET, "/site.css").await;
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );

    let resp = send(&app, Method::GET, "/about/").await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = send(&app, Method::HEAD, "/index.html").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()[header::CONTENT_LENGTH], "4");
}

#[tokio::test]
async fn symlinks_inside_the_root_are_followed() {
    let (_tmp, app) = fixture();
    let resp = send(&app, Method::GET, "/inside-link.txt").await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body(resp).await, "home");
}

#[tokio::test]
async fn symlinks_cannot_escape_the_root() {
    let (_tmp, app) = fixture();
    for uri in ["/escape.txt", "/escape-dir/secret.txt", "/escape-dir/"] {
        let resp = send(&app, Method::GET, uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn traversal_is_rejected() {
    let (_tmp, app) = fixture();
    for uri in [
        "/../outside/secret.txt",
        "/%2e%2e/outside/secret.txt",
        "/docs/%2E%2E/%2E%2E/outside/secret.txt",
    ] {
        let resp = send(&app, Method::GET, uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn directories_redirect_to_a_trailing_slash_on_site() {
    let (_tmp, app) = fixture();

    let resp = send(&app, Method::GET, "/docs?x=1").await;
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(resp.headers()[header::LOCATION], "/docs/?x=1");

    let resp = send(&app, Method::GET, "//evil.com").await;
    assert_eq!(resp.headers()[header::LOCATION], "/evil.com/");
}

#[tokio::test]
async fn only_get_and_head_are_allowed() {
    let (_tmp, app) = fixture();
    let resp = send(&app, Method::POST, "/index.html").await;
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers()[header::ALLOW], "GET, HEAD");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
}

#[test]
fn root_is_resolved_at_build_time() {
    let tmp = TempDir::new().unwrap();
    let file = tmp.path().join("file.txt");
    std::fs::write(&file, "").unwrap();

    let err = Server::new().static_dir(&file).build().unwrap_err();
    assert!(matches!(err, webnook::Error::StaticRoot { .. }));
    assert!(Server::new()
        .static_dir(Path::new(tmp.path()))
        .build()
        .is_ok());
}

#[tokio::test]
async fn static_files_get_cache_policy_and_etags() {
    let tmp = TempDir::new().unwrap();
    let site = tmp.path();
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::write(site.join("index.html"), "home").unwrap();
    std::fs::write(site.join("assets/app.js"), "let a;").unwrap();
    let app = Server::new()
        .static_dir(site)
        .cache("/assets/", Cache::Immutable)
        .build()
        .unwrap();

    let resp = send(&app, Method::GET, "/").await;
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
    let etag = resp.headers()[header::ETAG].clone();
    let last_modified = resp.headers()[header::LAST_MODIFIED].clone();
    assert!(etag.to_str().unwrap().starts_with("W/"));

    let resp = send(&app, Method::GET, "/assets/app.js").await;
    assert_eq!(
        resp.headers()[header::CACHE_CONTROL],
        "public, max-age=31536000, immutable"
    );

    let cond = Request::builder()
        .uri("/")
        .header(header::IF_NONE_MATCH, etag)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(cond).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
    assert_eq!(resp.headers()[header::LAST_MODIFIED], last_modified);
    assert_eq!(resp.headers()[header::ACCEPT_RANGES], "bytes");
}

#[tokio::test]
async fn precompressed_siblings_are_served() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("site.css"), "body {}").unwrap();
    std::fs::write(tmp.path().join("site.css.gz"), "gzipped-bytes").unwrap();
    let app = Server::new().static_dir(tmp.path()).build().unwrap();

    let req = Request::builder()
        .uri("/site.css")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(resp.headers()[header::VARY], "accept-encoding");
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/css; charset=utf-8"
    );
    assert_eq!(body(resp).await, "gzipped-bytes");

    let resp = send(&app, Method::GET, "/site.css").await;
    assert_eq!(body(resp).await, "body {}");
}

#[tokio::test]
async fn precompressed_symlinks_are_confined_and_safe_fallbacks_work() {
    for extension in ["gz", "br"] {
        for hidden in [false, true] {
            let tmp = TempDir::new().unwrap();
            let site = tmp.path().join("site");
            std::fs::create_dir(&site).unwrap();
            let secret = if hidden {
                site.join(".env")
            } else {
                tmp.path().join("secret.txt")
            };
            std::fs::write(&secret, "secret").unwrap();
            std::fs::write(site.join("site.css"), "body {}").unwrap();
            symlink(&secret, site.join(format!("site.css.{extension}"))).unwrap();
            let app = Server::new().static_dir(&site).build().unwrap();
            let encoding = if extension == "gz" { "gzip" } else { "br" };

            for method in [Method::GET, Method::HEAD] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.clone())
                            .uri("/site.css")
                            .header(header::ACCEPT_ENCODING, encoding)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
                if method == Method::GET {
                    assert_eq!(body(response).await, "body {}");
                }
            }
        }
    }
}

#[tokio::test]
async fn compressed_variants_respect_preferences_and_revalidate() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("site.css"), "body {}").unwrap();
    std::fs::write(tmp.path().join("safe.gz"), "safe-gzip").unwrap();
    symlink(tmp.path().join("safe.gz"), tmp.path().join("site.css.gz")).unwrap();
    std::fs::write(tmp.path().join("site.css.br"), "safe-brotli").unwrap();
    let app = Server::new().static_dir(tmp.path()).build().unwrap();

    for (accept, encoding, expected) in [
        ("gzip, br", Some("br"), "safe-brotli"),
        ("br;q=0.1, gzip;q=0.9", Some("gzip"), "safe-gzip"),
        ("br;q=0, gzip;q=0", None, "body {}"),
        ("identity;q=1, gzip;q=0.5", None, "body {}"),
        ("*;q=1, br;q=0", Some("gzip"), "safe-gzip"),
    ] {
        let request = || {
            Request::builder()
                .uri("/site.css")
                .header(header::ACCEPT_ENCODING, accept)
        };
        let response = app
            .clone()
            .oneshot(request().body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{accept}");
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .map(|v| v.to_str().unwrap()),
            encoding,
            "{accept}"
        );
        let etag = response.headers()[header::ETAG].clone();
        let last_modified = response.headers()[header::LAST_MODIFIED].clone();
        assert_eq!(body(response).await, expected);

        let response = app
            .clone()
            .oneshot(
                request()
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::VARY], "accept-encoding");
        assert_eq!(response.headers()[header::LAST_MODIFIED], last_modified);
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
    }
}

#[tokio::test]
async fn unsafe_preferred_variant_falls_back_only_to_acceptable_files() {
    let tmp = TempDir::new().unwrap();
    let site = tmp.path().join("site");
    std::fs::create_dir(&site).unwrap();
    std::fs::write(tmp.path().join("secret"), "secret").unwrap();
    std::fs::write(site.join("site.css"), "body {}").unwrap();
    std::fs::write(site.join("site.css.gz"), "safe-gzip").unwrap();
    symlink(tmp.path().join("secret"), site.join("site.css.br")).unwrap();
    let app = Server::new().static_dir(&site).build().unwrap();

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/site.css")
                .header(header::ACCEPT_ENCODING, "br, gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(body(response).await, "safe-gzip");

    let response = app
        .oneshot(
            Request::builder()
                .uri("/site.css")
                .header(header::ACCEPT_ENCODING, "br, identity;q=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
}

#[tokio::test]
async fn etags_take_precedence_over_modification_dates() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("index.html");
    std::fs::write(&path, "before").unwrap();
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let app = Server::new().static_dir(tmp.path()).build().unwrap();
    let response = send(&app, Method::GET, "/").await;
    let etag = response.headers()[header::ETAG].clone();
    let since = response.headers()[header::LAST_MODIFIED].clone();

    // A deployment can preserve timestamps while changing the file's content.
    std::fs::write(&path, "after: different length").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();

    for method in [Method::GET, Method::HEAD] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method.clone())
                    .uri("/")
                    .header(header::IF_NONE_MATCH, &etag)
                    .header(header::IF_MODIFIED_SINCE, &since)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_ne!(response.headers()[header::ETAG], etag);
        if method == Method::GET {
            assert_eq!(body(response).await, "after: different length");
        }
    }

    // Date-only conditional requests continue to work.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/")
                .header(header::IF_MODIFIED_SINCE, since)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn hidden_files_are_not_served() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::create_dir_all(root.join(".well-known/acme-challenge")).unwrap();
    std::fs::write(root.join(".env"), "SECRET=1").unwrap();
    std::fs::write(root.join(".git/config"), "[core]").unwrap();
    std::fs::write(root.join(".well-known/acme-challenge/token"), "proof").unwrap();
    symlink(root.join(".env"), root.join("env.txt")).unwrap();
    let app = Server::new().static_dir(root).build().unwrap();

    for uri in ["/.env", "/.git/config", "/.git/", "/env.txt", "/%2eenv"] {
        let resp = send(&app, Method::GET, uri).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
    let resp = send(&app, Method::GET, "/.well-known/acme-challenge/token").await;
    assert_eq!(body(resp).await, "proof");
}
