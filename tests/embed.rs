//! `rust-embed` integration (requires the `embed` feature).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rust_embed::RustEmbed;
use tower::ServiceExt;
use webnook::{Assets, Server};

#[derive(RustEmbed)]
#[folder = "examples/assets/public/"]
struct Public;

#[tokio::test]
async fn embedded_folder_is_served() {
    let app = Server::new()
        .assets(Assets::new().embed::<Public>())
        .build()
        .unwrap();

    for (uri, content_type) in [
        ("/", "text/html; charset=utf-8"),
        ("/site.css", "text/css; charset=utf-8"),
        ("/404", "text/html; charset=utf-8"),
    ] {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        assert_eq!(resp.headers()[header::CONTENT_TYPE], content_type, "{uri}");
    }
}
