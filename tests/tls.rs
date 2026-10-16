//! Direct HTTPS tests (requires the `tls` and `http-client` features, the
//! combination that pulls in more than one rustls crypto provider).

use std::path::PathBuf;

use axum::routing::get;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use webnook::{Error, Server};

struct Pems {
    _dir: TempDir,
    cert: PathBuf,
    key: PathBuf,
    cert_pem: String,
}

fn self_signed() -> Pems {
    let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let dir = TempDir::new().unwrap();
    let cert = dir.path().join("fullchain.pem");
    let key = dir.path().join("privkey.pem");
    let cert_pem = generated.cert.pem();
    std::fs::write(&cert, &cert_pem).unwrap();
    std::fs::write(&key, generated.key_pair.serialize_pem()).unwrap();
    Pems {
        _dir: dir,
        cert,
        key,
        cert_pem,
    }
}

#[tokio::test]
async fn serves_https() {
    let pems = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(
        Server::new()
            .on("/", get(|| async { "secure" }))
            .tls(&pems.cert, &pems.key)
            .listener(listener)
            .shutdown_signal(async {
                let _ = stopped.await;
            })
            .serve(),
    );

    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(pems.cert_pem.as_bytes()).unwrap())
        .resolve("localhost", addr)
        .build()
        .unwrap();
    let resp = client
        .get(format!("https://localhost:{}/", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.headers()["strict-transport-security"],
        "max-age=31536000"
    );
    assert_eq!(resp.text().await.unwrap(), "secure");

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

async fn serve_err(cert: PathBuf, key: PathBuf) -> Error {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    Server::new()
        .tls(cert, key)
        .listener(listener)
        .serve()
        .await
        .unwrap_err()
}

#[tokio::test]
async fn missing_certificate_is_a_startup_error() {
    let pems = self_signed();
    let err = serve_err(pems.cert.with_file_name("missing.pem"), pems.key.clone()).await;
    assert!(
        matches!(
            err,
            Error::Tls(webnook::TlsError::Read {
                kind: "certificate",
                ..
            })
        ),
        "{err}"
    );
}

#[tokio::test]
async fn certificate_without_pem_items_is_a_startup_error() {
    let pems = self_signed();
    std::fs::write(&pems.cert, "not a certificate").unwrap();
    let err = serve_err(pems.cert.clone(), pems.key.clone()).await;
    assert!(
        matches!(err, Error::Tls(webnook::TlsError::Empty("certificate", _))),
        "{err}"
    );
}

#[tokio::test]
async fn mismatched_key_is_a_startup_error() {
    let pems = self_signed();
    let other = self_signed();
    let err = serve_err(pems.cert.clone(), other.key.clone()).await;
    assert!(
        matches!(err, Error::Tls(webnook::TlsError::Invalid(_))),
        "{err}"
    );
}

#[tokio::test]
async fn http_listener_redirects_to_the_canonical_host() {
    let pems = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect_port = TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(
        Server::new()
            .tls(&pems.cert, &pems.key)
            .https_redirect("example.com", format!("127.0.0.1:{redirect_port}"))
            .listener(listener)
            .shutdown_signal(async {
                let _ = stopped.await;
            })
            .serve(),
    );

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut location = None;
    for _ in 0..50 {
        if let Ok(resp) = client
            .get(format!("http://127.0.0.1:{redirect_port}/"))
            .send()
            .await
        {
            assert_eq!(resp.status(), reqwest::StatusCode::MOVED_PERMANENTLY);
            location = Some(resp.headers()["location"].to_str().unwrap().to_string());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(location.as_deref(), Some("https://example.com/"));

    let resp = client
        .get(format!("http://127.0.0.1:{redirect_port}/a/b?c=d"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["location"], "https://example.com/a/b?c=d");

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn redirect_configuration_is_validated() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let err = Server::new()
        .https_redirect("example.com", "127.0.0.1:0")
        .listener(listener)
        .serve()
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Config(_)), "{err}");

    let pems = self_signed();
    for host in ["evil.com/path", "user@example.com", "https://example.com"] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let err = Server::new()
            .tls(&pems.cert, &pems.key)
            .https_redirect(host, "127.0.0.1:0")
            .listener(listener)
            .serve()
            .await
            .unwrap_err();
        assert!(matches!(err, Error::Config(_)), "{host}: {err}");
    }
}

#[cfg(unix)]
fn client_trusting(cert_pem: &str, addr: std::net::SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
        .resolve("localhost", addr)
        .pool_max_idle_per_host(0)
        .build()
        .unwrap()
}

#[cfg(unix)]
#[tokio::test]
async fn sighup_reloads_the_certificate() {
    let first = self_signed();
    let second = self_signed();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("https://localhost:{}/", addr.port());
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(
        Server::new()
            .on("/", get(|| async { "secure" }))
            .tls(&first.cert, &first.key)
            .listener(listener)
            .shutdown_signal(async {
                let _ = stopped.await;
            })
            .serve(),
    );

    let old = client_trusting(&first.cert_pem, addr);
    let new = client_trusting(&second.cert_pem, addr);
    assert!(old.get(&url).send().await.is_ok());
    assert!(new.get(&url).send().await.is_err());

    // An invalid replacement is rejected and the current certificate stays.
    std::fs::write(&first.cert, "garbage").unwrap();
    hangup();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(old.get(&url).send().await.is_ok());

    std::fs::copy(&second.cert, &first.cert).unwrap();
    std::fs::copy(&second.key, &first.key).unwrap();
    hangup();
    let mut reloaded = false;
    for _ in 0..50 {
        if new.get(&url).send().await.is_ok() {
            reloaded = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(reloaded, "new certificate must be served after SIGHUP");
    assert!(old.get(&url).send().await.is_err());

    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[cfg(unix)]
fn hangup() {
    let status = std::process::Command::new("kill")
        .args(["-HUP", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
}
