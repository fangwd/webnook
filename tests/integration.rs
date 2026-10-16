//! Integration tests exercising routing, asset headers, conditional requests,
//! limits, and configuration errors. Requests are driven through the built
//! [`axum::Router`] with `tower::ServiceExt::oneshot`, so no port is bound.

use axum::body::{Body, Bytes};
use axum::http::{header, Request, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tower::ServiceExt;
use webnook::{Asset, Assets, Cache, Error, Limits, Server};

async fn body_string(resp: Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

fn req(method: axum::http::Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap()
}

#[tokio::test]
async fn static_dir_serves_index_and_assets() {
    let app: Router = Server::new()
        .static_dir("./examples/assets/public")
        .build()
        .unwrap();

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(body_string(resp).await.contains("Ada Lovelace"));

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/site.css"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/css; charset=utf-8"
    );

    let resp = app
        .oneshot(req(axum::http::Method::GET, "/missing"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn explicit_route_wins_over_fallback() {
    let assets = Assets::new().add("/index.html", "asset home");

    let app = Server::new()
        .assets(assets)
        .on("/", get(|| async { "route home" }))
        .build()
        .unwrap();

    let resp = app
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "route home");
}

#[tokio::test]
async fn method_mismatch_returns_405_and_does_not_fall_through() {
    let assets = Assets::new().add("/index.html", "asset home");

    let app = Server::new()
        .assets(assets)
        .on("/api/foo", get(|| async { "foo" }))
        .build()
        .unwrap();

    // Registered route rejects the method.
    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::POST, "/api/foo"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Known embedded asset rejects a non-GET/HEAD method.
    let resp = app
        .oneshot(req(axum::http::Method::POST, "/"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(resp.headers()[header::ALLOW], "GET, HEAD");
    assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn embedded_and_disk_files_negotiate_encodings_the_same_way() {
    use std::io::Write;

    let css = "body { color: red; }".repeat(100);
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("site.css"), &css).unwrap();
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(css.as_bytes()).unwrap();
    std::fs::write(directory.path().join("site.css.gz"), gzip.finish().unwrap()).unwrap();
    let apps = [
        Server::new()
            .assets(Assets::new().add("/site.css", css))
            .build()
            .unwrap(),
        Server::new().static_dir(directory.path()).build().unwrap(),
    ];

    for (accept, expected_status, expected_encoding) in [
        ("x-gzip", StatusCode::OK, Some("gzip")),
        ("gzip;q=abc", StatusCode::OK, None),
        ("gzip;q=1.5", StatusCode::OK, None),
        ("gzip;q=1e0", StatusCode::OK, None),
        ("identity;q=1, gzip;q=0.5", StatusCode::OK, None),
        ("gzip;q=0, *", StatusCode::OK, None),
        ("*;q=0", StatusCode::NOT_ACCEPTABLE, None),
        ("br, identity;q=0", StatusCode::NOT_ACCEPTABLE, None),
        ("gzip;q=0, gzip;q=1", StatusCode::OK, Some("gzip")),
    ] {
        for app in &apps {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/site.css")
                        .header(header::ACCEPT_ENCODING, accept)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status, "{accept}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .map(|value| value.to_str().unwrap()),
                expected_encoding,
                "{accept}"
            );
        }
    }
}

#[tokio::test]
async fn embedded_asset_etag_conditional_and_head() {
    let assets = Assets::new().add("/index.html", "<h1>hi</h1>");

    let app = Server::new().assets(assets).build().unwrap();

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-cache"
    );
    let etag = resp
        .headers()
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // Conditional request with a matching ETag.
    let cond = Request::builder()
        .uri("/")
        .header(header::IF_NONE_MATCH, &etag)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(cond).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);

    // HEAD returns headers with content-length but an empty body.
    let resp = app
        .oneshot(req(axum::http::Method::HEAD, "/"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get(header::CONTENT_LENGTH).unwrap(), "11");
    assert!(body_string(resp).await.is_empty());
}

#[tokio::test]
async fn custom_404_uses_real_status() {
    let assets = Assets::new().add("/index.html", "home");

    let app = Server::new()
        .assets(assets)
        .not_found("<h1>nope</h1>")
        .build()
        .unwrap();
    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/does-not-exist"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(
        resp.headers()[header::CONTENT_TYPE],
        "text/html; charset=utf-8"
    );
    assert!(resp.headers().get(header::ETAG).is_none());
    assert_eq!(body_string(resp).await, "<h1>nope</h1>");

    // A validator must never turn the 404 page into an empty response.
    let cond = Request::builder()
        .uri("/does-not-exist")
        .header(header::IF_NONE_MATCH, "*")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(cond).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert_eq!(body_string(resp).await, "<h1>nope</h1>");
}

#[tokio::test]
async fn body_limit_rejects_oversized_requests() {
    let app = Server::new()
        .limits(Limits {
            max_body_bytes: 10,
            ..Limits::default()
        })
        .on("/x", post(|_b: Bytes| async { "ok" }))
        .build()
        .unwrap();

    let big = Request::builder()
        .method(axum::http::Method::POST)
        .uri("/x")
        .body(Body::from("x".repeat(64)))
        .unwrap();
    let resp = app.oneshot(big).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn body_limit_also_caps_raw_and_streamed_bodies() {
    let app = Server::new()
        .limits(Limits {
            max_body_bytes: 10,
            ..Limits::default()
        })
        .on(
            "/raw",
            post(|body: Body| async move {
                match axum::body::to_bytes(body, usize::MAX).await {
                    Ok(bytes) => (StatusCode::OK, bytes.len().to_string()),
                    Err(_) => (StatusCode::PAYLOAD_TOO_LARGE, String::new()),
                }
            })
            .layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .build()
        .unwrap();

    for announced in [true, false] {
        for size in [10, 11] {
            let mut request = Request::builder().method("POST").uri("/raw");
            if announced {
                request = request.header(header::CONTENT_LENGTH, size);
            }
            // A stream has no known size hint, even when Content-Length is set.
            let chunks = [size / 2, size - size / 2]
                .map(|len| Ok::<_, std::convert::Infallible>(Bytes::from("x".repeat(len))));
            let response = app
                .clone()
                .oneshot(
                    request
                        .body(Body::from_stream(futures_util::stream::iter(chunks)))
                        .unwrap(),
                )
                .await
                .unwrap();
            let expected = if size > 10 {
                StatusCode::PAYLOAD_TOO_LARGE
            } else {
                StatusCode::OK
            };
            assert_eq!(
                response.status(),
                expected,
                "announced={announced}, size={size}"
            );
        }
    }
}

#[tokio::test]
async fn announced_oversized_body_is_rejected_before_the_handler() {
    async fn unexpected_handler() -> StatusCode {
        panic!("oversized request reached handler");
    }

    let app = Server::new()
        .limits(Limits {
            max_body_bytes: 10,
            ..Limits::default()
        })
        .on("/", post(unexpected_handler))
        .build()
        .unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/")
                .header(header::CONTENT_LENGTH, "11")
                .body(Body::from("x".repeat(11)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn security_headers_are_applied() {
    let app = Server::new()
        .on("/", get(|| async { "ok" }))
        .build()
        .unwrap();
    let resp = app
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    let h = resp.headers();
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["referrer-policy"], "strict-origin-when-cross-origin");
    assert_eq!(h["x-frame-options"], "SAMEORIGIN");
    assert!(h.get("strict-transport-security").is_none());
}

#[tokio::test]
async fn security_headers_can_be_changed_and_removed() {
    let app = Server::new()
        .header("content-security-policy", "default-src 'self'")
        .header(header::X_FRAME_OPTIONS, "DENY")
        .remove_header("referrer-policy")
        .on("/", get(|| async { "ok" }))
        .on(
            "/own",
            get(|| async { ([(header::X_FRAME_OPTIONS, "ALLOWALL")], "own") }),
        )
        .build()
        .unwrap();

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    let h = resp.headers();
    assert_eq!(h["content-security-policy"], "default-src 'self'");
    assert_eq!(h["x-frame-options"], "DENY");
    assert!(h.get("referrer-policy").is_none());

    let resp = app
        .oneshot(req(axum::http::Method::GET, "/own"))
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-frame-options"], "ALLOWALL");
}

#[test]
fn invalid_header_is_a_config_error() {
    let err = Server::new().header("bad header", "x").build().unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err}");
    let err = Server::new()
        .header("x-ok", "bad\nvalue")
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err}");
}

#[tokio::test]
async fn embedded_paths_resolve_like_a_static_site() {
    let assets = Assets::new()
        .add("/index.html", "home")
        .add("/about.html", "about")
        .add("/blog/index.html", "blog")
        .add("/robots.txt", "User-agent: *");
    let app = Server::new().assets(assets).build().unwrap();

    for (uri, expected) in [
        ("/", "home"),
        ("/index.html", "home"),
        ("/about", "about"),
        ("/about.html", "about"),
        ("/blog/", "blog"),
        ("/robots.txt", "User-agent: *"),
    ] {
        let resp = app
            .clone()
            .oneshot(req(axum::http::Method::GET, uri))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(body_string(resp).await, expected, "{uri}");
    }

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/blog?page=2"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
    assert_eq!(resp.headers()[header::LOCATION], "/blog/?page=2");

    for uri in ["/about/", "/missing", "/../index.html"] {
        let resp = app
            .clone()
            .oneshot(req(axum::http::Method::GET, uri))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn embedded_content_types_and_overrides() {
    let assets = Assets::new()
        .add("/index.html", "home")
        .add(
            "/data.bin",
            Asset::new(vec![1u8, 2, 3]).content_type("application/x-custom"),
        )
        .add("/app-1a2b.js", Asset::new("let x;").cache(Cache::Immutable));
    let app = Server::new().assets(assets).build().unwrap();

    for (uri, content_type, cache) in [
        ("/", "text/html; charset=utf-8", "no-cache"),
        ("/data.bin", "application/x-custom", "public, max-age=60"),
        (
            "/app-1a2b.js",
            "text/javascript; charset=utf-8",
            "public, max-age=31536000, immutable",
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(req(axum::http::Method::GET, uri))
            .await
            .unwrap();
        assert_eq!(resp.headers()[header::CONTENT_TYPE], content_type, "{uri}");
        assert_eq!(resp.headers()[header::CACHE_CONTROL], cache, "{uri}");
    }
}

#[tokio::test]
async fn weak_etag_revalidates() {
    let app = Server::new()
        .assets(Assets::new().add("/index.html", "home"))
        .build()
        .unwrap();
    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    let etag = resp.headers()[header::ETAG].to_str().unwrap().to_string();

    let cond = Request::builder()
        .uri("/")
        .header(header::IF_NONE_MATCH, format!("W/{etag}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(cond).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
}

#[tokio::test]
async fn on_merges_methods_for_the_same_path() {
    let app = Server::new()
        .on("/form", get(|| async { "show" }))
        .on("/form", post(|| async { "submit" }))
        .build()
        .unwrap();

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/form"))
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "show");
    let resp = app
        .oneshot(req(axum::http::Method::POST, "/form"))
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "submit");
}

#[test]
#[should_panic(expected = "Overlapping method route")]
fn duplicate_method_on_same_path_panics() {
    let _ = Server::new()
        .on("/dup", get(|| async { "a" }))
        .on("/dup", get(|| async { "b" }));
}

#[test]
#[should_panic(expected = "Overlapping method route")]
fn router_overlapping_on_route_panics() {
    let _ = Server::new()
        .on("/dup", get(|| async { "a" }))
        .router(Router::new().route("/dup", get(|| async { "b" })));
}

#[tokio::test]
async fn router_can_be_merged_more_than_once() {
    let app = Server::new()
        .router(Router::new().route("/a", get(|| async { "a" })))
        .router(Router::new().route("/b", get(|| async { "b" })))
        .build()
        .unwrap();

    for path in ["/a", "/b"] {
        let resp = app
            .clone()
            .oneshot(req(axum::http::Method::GET, path))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
}

#[tokio::test]
async fn concurrency_limit_is_shared_across_routes() {
    let release = Arc::new(Notify::new());
    let held = release.clone();
    let app = Server::new()
        .limits(Limits {
            max_concurrency: 1,
            ..Limits::default()
        })
        .on(
            "/hold",
            get(move || {
                let held = held.clone();
                async move {
                    held.notified().await;
                    "released"
                }
            }),
        )
        .on("/other", get(|| async { "other" }))
        .build()
        .unwrap();

    let holder = tokio::spawn(app.clone().oneshot(req(axum::http::Method::GET, "/hold")));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // A different route must wait for the single global slot.
    let other = tokio::spawn(app.oneshot(req(axum::http::Method::GET, "/other")));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!other.is_finished(), "limit must apply across routes");

    release.notify_one();
    assert_eq!(
        body_string(holder.await.unwrap().unwrap()).await,
        "released"
    );
    assert_eq!(body_string(other.await.unwrap().unwrap()).await, "other");
}

#[tokio::test]
async fn request_timeout_returns_503() {
    let app = Server::new()
        .limits(Limits {
            request_timeout: Duration::from_millis(50),
            ..Limits::default()
        })
        .on(
            "/slow",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                "late"
            }),
        )
        .build()
        .unwrap();

    let resp = app
        .oneshot(req(axum::http::Method::GET, "/slow"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn missing_bind_address_is_a_config_error() {
    let err = Server::new().serve().await.unwrap_err();
    assert!(matches!(err, Error::NoBindAddress));
}

#[tokio::test]
async fn invalid_static_root_is_a_config_error() {
    let err = Server::new()
        .static_dir("./definitely-not-here")
        .build()
        .unwrap_err();
    assert!(matches!(err, Error::StaticRoot { .. }));
}

#[tokio::test]
async fn cache_rules_apply_by_longest_prefix() {
    let assets = Assets::new()
        .add("/assets/app.js", "let a;")
        .add("/assets/fonts/x.woff2", vec![0u8; 4])
        .add(
            "/assets/pinned.css",
            Asset::new("a{}").cache(Cache::NoStore),
        );
    let app = Server::new()
        .assets(assets)
        .cache("/assets/", Cache::MaxAge(600))
        .cache("/assets/fonts/", Cache::Immutable)
        .build()
        .unwrap();

    for (uri, cache) in [
        ("/assets/app.js", "public, max-age=600"),
        (
            "/assets/fonts/x.woff2",
            "public, max-age=31536000, immutable",
        ),
        ("/assets/pinned.css", "no-store"),
    ] {
        let resp = app
            .clone()
            .oneshot(req(axum::http::Method::GET, uri))
            .await
            .unwrap();
        assert_eq!(resp.headers()[header::CACHE_CONTROL], cache, "{uri}");
    }
}

#[tokio::test]
async fn compressible_assets_are_served_gzipped_when_accepted() {
    use std::io::Read;

    let page = format!("<!doctype html>{}", "<p>hello webnook</p>".repeat(200));
    let app = Server::new()
        .assets(Assets::new().add("/index.html", page.clone()))
        .build()
        .unwrap();

    let gz = Request::builder()
        .uri("/")
        .header(header::ACCEPT_ENCODING, "gzip, br")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(gz).await.unwrap();
    assert_eq!(resp.headers()[header::CONTENT_ENCODING], "gzip");
    assert_eq!(resp.headers()[header::VARY], "accept-encoding");
    let gz_etag = resp.headers()[header::ETAG].clone();
    let compressed = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(compressed.len() < page.len() / 4);
    let mut decoded = String::new();
    flate2::read::GzDecoder::new(&compressed[..])
        .read_to_string(&mut decoded)
        .unwrap();
    assert_eq!(decoded, page);

    let resp = app
        .clone()
        .oneshot(req(axum::http::Method::GET, "/"))
        .await
        .unwrap();
    assert!(resp.headers().get(header::CONTENT_ENCODING).is_none());
    assert_eq!(resp.headers()[header::VARY], "accept-encoding");
    assert_ne!(resp.headers()[header::ETAG], gz_etag);
    assert_eq!(body_string(resp).await, page);

    let app = Server::new()
        .assets(Assets::new().add("/index.html", page))
        .compress(false)
        .build()
        .unwrap();
    let gz = Request::builder()
        .uri("/")
        .header(header::ACCEPT_ENCODING, "gzip")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(gz).await.unwrap();
    assert!(resp.headers().get(header::CONTENT_ENCODING).is_none());
}

#[tokio::test]
async fn route_specific_limits_take_precedence() {
    use axum::extract::DefaultBodyLimit;
    use tower_http::timeout::TimeoutLayer;

    let app = Server::new()
        .on(
            "/tight",
            post(|_b: Bytes| async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                "ok"
            })
            .layer((
                DefaultBodyLimit::max(8),
                TimeoutLayer::with_status_code(
                    StatusCode::SERVICE_UNAVAILABLE,
                    Duration::from_millis(50),
                ),
            )),
        )
        .on("/loose", post(|_b: Bytes| async { "ok" }))
        .build()
        .unwrap();

    let post_body = |uri: &str, len: usize| {
        Request::builder()
            .method(axum::http::Method::POST)
            .uri(uri)
            .body(Body::from("x".repeat(len)))
            .unwrap()
    };
    let resp = app.clone().oneshot(post_body("/tight", 64)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let resp = app.clone().oneshot(post_body("/tight", 4)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let resp = app.oneshot(post_body("/loose", 64)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn handler_panics_become_generic_500s() {
    let app = Server::new()
        .on(
            "/boom",
            get(|| async { panic!("database password is hunter2") as &'static str }),
        )
        .build()
        .unwrap();
    let resp = app
        .oneshot(req(axum::http::Method::GET, "/boom"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert!(!body_string(resp).await.contains("hunter2"));
}

#[tokio::test]
async fn client_ip_works_with_mock_connect_info() {
    use axum::extract::connect_info::MockConnectInfo;
    use std::net::SocketAddr;
    use webnook::ClientIp;

    let app = Server::new()
        .on(
            "/ip",
            get(|ClientIp(ip): ClientIp| async move { ip.to_string() }),
        )
        .build()
        .unwrap()
        .layer(MockConnectInfo(SocketAddr::from(([192, 0, 2, 1], 4000))));
    let resp = app
        .oneshot(req(axum::http::Method::GET, "/ip"))
        .await
        .unwrap();
    assert_eq!(body_string(resp).await, "192.0.2.1");
}
