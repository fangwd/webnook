//! Socket-level tests: connection limits, header timeouts, client addresses,
//! and graceful shutdown. Each test serves on an ephemeral port.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use axum::extract::ConnectInfo;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use webnook::{Limits, Result, Server};

struct Running {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<()>>,
}

async fn start(server: Server) -> Running {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(
        server
            .listener(listener)
            .shutdown_signal(async {
                let _ = stopped.await;
            })
            .serve(),
    );
    Running { addr, stop, task }
}

async fn send(addr: SocketAddr, path: &str) -> TcpStream {
    let mut stream = TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: test\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn read_all(mut stream: TcpStream) -> String {
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf).await;
    String::from_utf8_lossy(&buf).into_owned()
}

async fn get_text(addr: SocketAddr, path: &str) -> String {
    read_all(send(addr, path).await).await
}

#[tokio::test]
async fn handlers_receive_the_client_address() {
    let server = Server::new().on(
        "/ip",
        get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.ip().to_string() }),
    );
    let running = start(server).await;

    let response = get_text(running.addr, "/ip").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("127.0.0.1"), "{response}");

    running.stop.send(()).unwrap();
    running.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_lets_in_flight_requests_finish() {
    let server = Server::new().on(
        "/slow",
        get(|| async {
            tokio::time::sleep(Duration::from_millis(300)).await;
            "done"
        }),
    );
    let running = start(server).await;

    let in_flight = send(running.addr, "/slow").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    running.stop.send(()).unwrap();

    let response = read_all(in_flight).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.ends_with("done"), "{response}");
    running.task.await.unwrap().unwrap();

    assert!(
        TcpStream::connect(running.addr).await.is_err(),
        "listener must be closed after shutdown"
    );
}

#[tokio::test]
async fn shutdown_grace_bounds_the_drain() {
    let server = Server::new()
        .limits(Limits {
            shutdown_grace: Duration::from_millis(200),
            ..Limits::default()
        })
        .on(
            "/stuck",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                "never"
            }),
        );
    let running = start(server).await;

    let in_flight = send(running.addr, "/stuck").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    running.stop.send(()).unwrap();

    tokio::time::timeout(Duration::from_secs(5), running.task)
        .await
        .expect("serve must return once the grace period elapses")
        .unwrap()
        .unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(read_all(in_flight).await.is_empty());
}

#[tokio::test]
async fn connection_limit_defers_excess_connections() {
    let server = Server::new()
        .limits(Limits {
            max_connections: 1,
            ..Limits::default()
        })
        .on("/", get(|| async { "ok" }));
    let running = start(server).await;

    // Occupies the only connection slot without sending a request.
    let idle = TcpStream::connect(running.addr).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut waiting = send(running.addr, "/").await;
    let mut byte = [0u8; 1];
    let early = tokio::time::timeout(Duration::from_millis(300), waiting.read(&mut byte)).await;
    assert!(early.is_err(), "second connection must not be served yet");

    drop(idle);
    let rest = tokio::time::timeout(Duration::from_secs(2), read_all(waiting))
        .await
        .expect("second connection is served once the slot frees");
    assert!(format!("{}{rest}", byte[0] as char).contains("200 OK"));

    running.stop.send(()).unwrap();
    running.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn slow_headers_are_disconnected() {
    let server = Server::new()
        .limits(Limits {
            header_read_timeout: Duration::from_millis(200),
            ..Limits::default()
        })
        .on("/", get(|| async { "ok" }));
    let running = start(server).await;

    let mut slow = TcpStream::connect(running.addr).await.unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n")
        .await
        .unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(2), read_all(slow)).await;
    assert!(
        closed.is_ok(),
        "connection must close after the header timeout"
    );

    running.stop.send(()).unwrap();
    running.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn bind_failures_name_the_address() {
    let taken = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = taken.local_addr().unwrap();

    let err = Server::new().bind(addr).serve().await.unwrap_err();
    assert!(
        matches!(&err, webnook::Error::Bind { addr: a, .. } if *a == addr.to_string()),
        "{err}"
    );

    let err = Server::new()
        .bind("not an address")
        .serve()
        .await
        .unwrap_err();
    assert!(
        err.to_string().starts_with("failed to bind not an address"),
        "{err}"
    );
}

#[tokio::test]
async fn bind_accepts_address_strings() {
    // Reserve a free port, release it, then bind to it by string.
    let port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(
        Server::new()
            .on("/", get(|| async { "ok" }))
            .bind(format!("127.0.0.1:{port}"))
            .shutdown_signal(async {
                let _ = stopped.await;
            })
            .serve(),
    );

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut response = String::new();
    for _ in 0..50 {
        if let Ok(stream) = TcpStream::connect(addr).await {
            stream.writable().await.unwrap();
            drop(stream);
            response = get_text(addr, "/").await;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(response.ends_with("ok"), "{response}");

    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn client_ip_honors_only_trusted_proxies() {
    use webnook::ClientIp;

    let handler = get(|ClientIp(ip): ClientIp| async move { ip.to_string() });
    let request = |addr: SocketAddr| async move {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(
                b"GET /ip HTTP/1.1\r\nHost: t\r\nX-Forwarded-For: 198.51.100.7\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        read_all(stream).await
    };

    let direct = start(Server::new().on("/ip", handler.clone())).await;
    assert!(request(direct.addr).await.ends_with("127.0.0.1"));
    direct.stop.send(()).unwrap();

    let proxied = start(Server::new().trust_proxy("127.0.0.0/8").on("/ip", handler)).await;
    assert!(request(proxied.addr).await.ends_with("198.51.100.7"));
    proxied.stop.send(()).unwrap();
}

#[test]
fn invalid_proxy_range_is_a_config_error() {
    let err = Server::new()
        .trust_proxy("10.0.0.0/99")
        .build()
        .unwrap_err();
    assert!(matches!(err, webnook::Error::Config(_)), "{err}");
}
