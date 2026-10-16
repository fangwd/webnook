//! Direct HTTPS without a reverse proxy, with an HTTP listener that
//! redirects to the canonical HTTPS origin.
//!
//! Try it locally with a self-signed certificate:
//!
//! ```sh
//! openssl req -x509 -newkey rsa:2048 -nodes -days 7 -subj '/CN=localhost' \
//!   -addext 'subjectAltName=DNS:localhost' \
//!   -keyout /tmp/privkey.pem -out /tmp/fullchain.pem
//! TLS_CERT=/tmp/fullchain.pem TLS_KEY=/tmp/privkey.pem \
//!   cargo run --example https --features tls
//! curl -k https://localhost:8443/
//! curl -i http://localhost:8080/about    # 301 to https://localhost:8443/about
//! ```
//!
//! In production, bind `[::]:443` and `[::]:80`, use the canonical host
//! (e.g. `example.com`), and point `TLS_CERT`/`TLS_KEY` at the files your
//! ACME client maintains. Send `SIGHUP` after renewal to reload them.

use webnook::{Assets, Server};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    let cert = std::env::var("TLS_CERT")?;
    let key = std::env::var("TLS_KEY")?;
    let https_addr = env_or("HTTPS_ADDR", "127.0.0.1:8443");
    let http_addr = env_or("HTTP_ADDR", "127.0.0.1:8080");
    let canonical_host = env_or("CANONICAL_HOST", "localhost:8443");

    let assets = Assets::new()
        .add("/index.html", include_str!("./assets/public/index.html"))
        .add("/about.html", include_str!("./assets/pages/about.html"))
        .add("/site.css", include_str!("./assets/public/site.css"));

    Server::new()
        .assets(assets)
        .tls(cert, key)
        .https_redirect(canonical_host, http_addr)
        .bind(https_addr)
        .serve()
        .await?;
    Ok(())
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}
