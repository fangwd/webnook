//! Outbound HTTP client tests (requires the `http-client` feature).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::StatusCode;
use axum::routing::get;
use axum::Json;
use futures_util::stream;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use webnook::{Error, HttpClient, HttpClientOptions, Server};

const CHUNK: usize = 16 * 1024;

#[derive(Default)]
struct Load {
    current: AtomicUsize,
    peak: AtomicUsize,
}

/// Upstream with `/chunked` and `/sized` bodies of `CHUNK * 8` bytes, JSON,
/// error, and slow endpoints.
async fn upstream(load: Arc<Load>) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = Server::new()
        .on(
            "/chunked",
            get(|| async {
                let chunks = (0..8).map(|_| Ok::<_, Infallible>(Bytes::from(vec![b'x'; CHUNK])));
                Body::from_stream(stream::iter(chunks))
            }),
        )
        .on("/sized", get(|| async { vec![b'x'; CHUNK * 8] }))
        .on(
            "/json",
            get(|| async { Json(serde_json::json!({ "ok": true })) }),
        )
        .on(
            "/fail",
            get(|| async { (StatusCode::BAD_GATEWAY, "secret provider detail") }),
        )
        .on(
            "/slow",
            get(move || {
                let load = load.clone();
                async move {
                    let now = load.current.fetch_add(1, Ordering::SeqCst) + 1;
                    load.peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    load.current.fetch_sub(1, Ordering::SeqCst);
                    "done"
                }
            }),
        )
        .listener(listener)
        .shutdown_signal(async {
            let _ = stopped.await;
        });
    tokio::spawn(server.serve());
    (addr, stop)
}

fn client(options: HttpClientOptions) -> HttpClient {
    HttpClient::new(options).unwrap()
}

fn limit(max_response_bytes: usize) -> HttpClient {
    client(HttpClientOptions {
        max_response_bytes,
        ..HttpClientOptions::default()
    })
}

#[tokio::test]
async fn body_within_limit_is_returned() {
    let (addr, _stop) = upstream(Arc::default()).await;
    let client = limit(CHUNK * 8);
    let resp = client
        .send(client.get(format!("http://{addr}/chunked")))
        .await
        .unwrap();
    assert_eq!(resp.status, StatusCode::OK);
    assert_eq!(resp.body.len(), CHUNK * 8);
}

#[tokio::test]
async fn unannounced_body_over_limit_is_rejected() {
    let (addr, _stop) = upstream(Arc::default()).await;
    let client = limit(CHUNK * 2);
    let err = client
        .send(client.get(format!("http://{addr}/chunked")))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::HttpBodyTooLarge(limit) if limit == CHUNK * 2));
}

#[tokio::test]
async fn announced_body_over_limit_is_rejected() {
    let (addr, _stop) = upstream(Arc::default()).await;
    let client = limit(CHUNK);
    let err = client
        .send(client.get(format!("http://{addr}/sized")))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::HttpBodyTooLarge(limit) if limit == CHUNK));
}

#[tokio::test]
async fn json_and_status_helpers() {
    let (addr, _stop) = upstream(Arc::default()).await;
    let client = client(HttpClientOptions::default());

    let value: serde_json::Value = client
        .send(client.get(format!("http://{addr}/json")))
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .unwrap();
    assert_eq!(value["ok"], true);

    let resp = client
        .send(client.get(format!("http://{addr}/fail")))
        .await
        .unwrap();
    assert_eq!(resp.status, StatusCode::BAD_GATEWAY);
    assert_eq!(resp.text(), "secret provider detail");
    let err = resp.error_for_status().unwrap_err();
    assert!(matches!(err, Error::HttpStatus(StatusCode::BAD_GATEWAY)));
    assert!(!err.to_string().contains("secret"));

    let err = client
        .send(client.get(format!("http://{addr}/sized")))
        .await
        .unwrap()
        .json::<serde_json::Value>()
        .unwrap_err();
    assert!(matches!(err, Error::HttpDecode(_)));
}

#[tokio::test]
async fn concurrent_requests_are_capped() {
    let load = Arc::new(Load::default());
    let (addr, _stop) = upstream(load.clone()).await;
    let client = client(HttpClientOptions {
        max_in_flight: 2,
        ..HttpClientOptions::default()
    });

    let sends = (0..6).map(|_| {
        let client = client.clone();
        tokio::spawn(async move { client.send(client.get(format!("http://{addr}/slow"))).await })
    });
    for send in sends.collect::<Vec<_>>() {
        assert_eq!(send.await.unwrap().unwrap().text(), "done");
    }
    assert_eq!(load.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn connection_failures_are_errors() {
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let client = client(HttpClientOptions::default());
    let err = client
        .send(client.get(format!("http://user:pw@127.0.0.1:{port}/x?api_key=secret")))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Http(_)), "{err}");
    let shown = format!("{err} {err:?}");
    assert!(shown.contains("127.0.0.1"), "{shown}");
    assert!(
        !shown.contains("secret") && !shown.contains("pw"),
        "{shown}"
    );
}
