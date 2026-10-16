# Webnook

Webnook is a small Rust library for running your own website.

You write an ordinary Rust program, hand Webnook your pages and routes, and
get **one binary** you can copy to a server. Webnook targets small Linux
hosts, including a 1 CPU, 512 MB RAM box; memory use depends on your assets
and handlers.

- **Pages and assets**: serve a folder, or embed it in the binary
- **Your own endpoints**: plain [Axum](https://docs.rs/axum) handlers
- **Safe by default**: request limits, timeouts, security headers, graceful shutdown
- **HTTPS built in** (optional): no Nginx or Caddy required
- **Extras when you need them**: SQLite and an outbound HTTP client

Webnook is not a CMS or a framework. Your content, HTML, CSS, and logic stay
yours.

## Contents

- [Quick start](#quick-start)
- [How it works](#how-it-works)
- [Optional features](#optional-features)
- [Examples](#examples)
- [Deploy to Ubuntu with HTTPS (certbot)](#deploy-to-ubuntu-with-https-certbot)
- [More deployment topics](#more-deployment-topics)

## Quick start

With this repository checked out as `webnook/`, create a sibling project
from its parent directory:

```sh
cargo new my-site
cd my-site
mkdir public
printf '<!doctype html><title>Home</title><h1>Hello!</h1>\n' > public/index.html
printf '<!doctype html><title>Not found</title><h1>Page not found</h1>\n' > public/404.html
```

Add these dependencies to `Cargo.toml`:

```toml
[dependencies]
webnook = { path = "../webnook" }
axum = "0.8"
serde_json = "1"
tokio = { version = "1", features = ["macros", "rt"] }
```

The examples use a local path because Webnook is not on crates.io yet.
Adjust `../webnook` to your checkout's location. The path is needed only
when building; the deployed binary does not need the checkout.

Write `src/main.rs`:

```rust,no_run
use axum::{routing::{get, post}, Json};
use serde_json::{json, Value};
use webnook::Server;

#[tokio::main(flavor = "current_thread")]
async fn main() -> webnook::Result<()> {
    Server::new()
        .static_dir("./public")
        .on("/api/hello", get(|| async { Json(json!({ "message": "Hello!" })) }))
        .on("/api/echo", post(|Json(body): Json<Value>| async move { Json(body) }))
        .bind("127.0.0.1:3000")
        .serve()
        .await
}
```

Run it with `cargo run` and open <http://127.0.0.1:3000>.

Try the JSON endpoints:

```sh
curl http://127.0.0.1:3000/api/hello
curl http://127.0.0.1:3000/api/echo -H 'Content-Type: application/json' -d '{"hello":"world"}'
```

## How it works

### Pages and assets

Give Webnook your files in one of two ways:

| | How | Good for |
| --- | --- | --- |
| **Folder on disk** | `.static_dir("./public")` | Development, or content you update without rebuilding |
| **Embedded in the binary** | `.assets(Assets::new().embed::<Public>())` | Production: one file to deploy |

Embedding uses the `embed` feature and `rust-embed`. Update the dependency
and add the derive crate:

```toml
webnook = { path = "../webnook", features = ["embed"] }
rust-embed = "8"
```

```rust,ignore
#[derive(rust_embed::RustEmbed)]
#[folder = "public/"]
struct Public;

Server::new().assets(webnook::Assets::new().embed::<Public>())
```

In debug builds the folder is still read from disk, so you can edit pages
and just restart. You can also add single files by hand:
`Assets::new().add("/about.html", include_str!("../pages/about.html"))`.

Both ways turn URLs into files the same way:

| URL | File |
| --- | --- |
| `/` | `index.html` |
| `/about` | `about.html` |
| `/blog/` | `blog/index.html` |
| `/blog` | redirects to `/blog/` |
| `/site.css` | `site.css` (sent as `text/css; charset=utf-8`) |
| anything else | `404 Not Found` |

Use `.not_found(include_str!("../public/404.html"))` for a custom 404 page.
From a folder on disk, hidden files such as `.env` or `.git/` are not
served (`.well-known` is allowed). The same root and hidden-file checks
apply to symlinks and precompressed siblings. Keep only public content in
the folder, and restrict who can modify it.

### Your own routes

Add endpoints with `.on(path, handler)`. Handlers are normal Axum handlers,
so extractors like `Json`, `Form`, and `State` all work. For larger apps,
build an Axum `Router` (with its state) and add it with `.router(router)`.

**Routes always win over files**, no matter the order you write them. If a
route exists but the method is wrong (say `POST` to a `GET` route), the
visitor gets `405 Method Not Allowed`.

### Caching and compression

- HTML is revalidated on every visit (`Cache-Control: no-cache` + ETag).
- Other files are cached for a minute by default.
- Files with a content hash in their name can be cached forever:
  `.cache("/assets/", Cache::Immutable)`.
- Embedded text files are gzipped once at startup and served compressed to
  browsers that support it. For folders, Webnook serves `site.css.gz` or
  `site.css.br` next to `site.css` if you create them. Update the original
  and compressed files together so they represent the same content.

### Built-in protections

Every server has limits that keep a small box healthy under load or abuse:

| Limit | Default | What happens |
| --- | --- | --- |
| Request body size | 2 MiB | `413 Payload Too Large` |
| Concurrent requests | 64 | Extra requests wait within the request timeout |
| Open connections | 256 | Extra connections wait in the OS queue |
| Time to send headers | 10 s | Slow clients are disconnected |
| Request timeout | 30 s | Queue wait and handler execution; `503` on expiry |
| Shutdown grace period | 10 s | In-flight requests may finish |

Change them with `.limits(Limits { ... })`. The body cap applies to all
body readers, including raw streams, and routes can only tighten it.
Oversized `Content-Length` values are rejected before the handler runs.
For bodies without a known length, reads fail at the cap: Axum's buffered
extractors return `413`; raw body handlers must handle the read error.
The request timeout ends when the handler returns a response; it does not
limit how long a visitor takes to download a streamed response.

Every application response also gets
`X-Content-Type-Options`, `Referrer-Policy`, and `X-Frame-Options` headers
(plus `Strict-Transport-Security` with HTTPS). Add your own with
`.header(name, value)` or remove one with `.remove_header(name)`.

With Rust's default panic unwinding, a handler panic returns a generic
`500`; panic details stay in server logs. A build using `panic = "abort"`
exits the process instead.

### Visitor IP addresses

Use the `ClientIp` extractor, e.g. for rate limiting a contact form. If you
run behind a proxy or load balancer, tell Webnook which address to trust
with `.trust_proxy("10.0.0.0/8")`; otherwise visitors could fake their IP.

### Logs and shutdown

Webnook logs through [`tracing`](https://docs.rs/tracing). Call
`tracing_subscriber::fmt().init()` in `main` to see the logs.

On `Ctrl-C` or `SIGTERM` (for example `systemctl stop`), Webnook stops
taking new connections, lets in-flight requests finish, and exits cleanly.

## Optional features

No optional features are enabled by default. Turn on what you need in
`Cargo.toml`, e.g.
`webnook = { path = "../webnook", features = ["tls", "embed"] }`.

| Feature | Adds |
| --- | --- |
| `embed` | Embed a folder in the binary with `Assets::embed` |
| `tls` | HTTPS with `.tls(cert, key)`, an HTTP→HTTPS redirect, and certificate reload |
| `sqlite` | `Sqlite`: a local database that won't block your server, with migrations |
| `http-client` | `HttpClient`: call outside APIs (email, newsletters) with timeouts and size limits |

## Examples

| Example | What it shows | Run it |
| --- | --- | --- |
| [`personal_site`](examples/personal_site.rs) | Folder, an embedded page, JSON endpoints | `cargo run --example personal_site` |
| [`embedded`](examples/embedded.rs) | Single binary, caching, custom 404 | `cargo run --example embedded --features embed` |
| [`startup_site`](examples/startup_site.rs) | Contact form saved to SQLite, with spam and CSRF protection | `cargo run --example startup_site --features sqlite` |
| [`newsletter`](examples/newsletter.rs) | Calling a provider API safely | `cargo run --example newsletter --features http-client` |
| [`https`](examples/https.rs) | HTTPS, HTTP redirect, certificate reload | see the comments at the top of the file |

The full API reference is in the Rust docs: `cargo doc --open --all-features`.

## Deploy to Ubuntu with HTTPS (certbot)

This walks through putting a site on a fresh Ubuntu 22.04 or 24.04 server,
served directly over HTTPS with a free Let's Encrypt certificate. No Nginx
needed. Replace `example.com` with your domain and `my-site` with your
project's name throughout.

**Before you start**, you need:

- A server running Ubuntu, reachable over SSH
- A domain whose DNS `A` record (and `AAAA` for IPv6) points at the server

### 1. Prepare your site for production

Start with the project and `public/index.html` and `public/404.html` files
from the quick start. Enable the features and add the crates the production
`main` uses:

```toml
[dependencies]
webnook = { path = "../webnook", features = ["tls", "embed"] }
axum = "0.8"
rust-embed = "8"
tokio = { version = "1", features = ["macros", "rt"] }
tower-http = { version = "0.6", features = ["fs"] }
tracing-subscriber = "0.3"
```

Then write `src/main.rs`. It embeds `public/`, serves HTTPS on port 443,
redirects port 80 to HTTPS, and answers certbot's renewal checks on HTTPS
after the redirect:

```rust,ignore
use axum::{routing::get, Router};
use rust_embed::RustEmbed;
use tower_http::services::ServeDir;
use webnook::{Assets, Server};

#[derive(RustEmbed)]
#[folder = "public/"]
struct Public;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    // e.g. "example.com", set in /etc/my-site/env
    let domain = std::env::var("SITE_DOMAIN")?;

    // certbot drops renewal check files here; see step 7.
    let acme = Router::new().nest_service(
        "/.well-known/acme-challenge",
        ServeDir::new("/var/lib/my-site/acme/.well-known/acme-challenge"),
    );

    Server::new()
        .assets(Assets::new().embed::<Public>())
        .not_found(include_str!("../public/404.html"))
        .router(acme)
        .on("/health", get(|| async { "ok" }))
        .tls("/etc/my-site/tls/fullchain.pem", "/etc/my-site/tls/privkey.pem")
        .https_redirect(domain, "[::]:80")
        .bind("[::]:443")
        .serve()
        .await?;
    Ok(())
}
```

> `[::]` listens on IPv6 and IPv4. If your server has IPv6 turned off,
> use `0.0.0.0:443` and `0.0.0.0:80` instead.

Run `cargo check` locally to validate the program and update `Cargo.lock`
with the production dependencies before building the release.

### 2. Build the binary

Compiling Rust needs more memory than a 512 MB server has, so build
somewhere else. The binary must be built on the **same Ubuntu version** as
the server (or older), because it links against the system C library.

Easiest options:

- **Another Ubuntu machine or VM** with the same release and CPU
  architecture: `cargo build --release --locked`
- **Docker** on your laptop (here for Ubuntu 24.04 on an x86 server):

  ```sh
  docker run --rm --platform linux/amd64 -v "$PWD/..":/workspace -w /workspace/my-site ubuntu:24.04 bash -c '
    apt-get update && apt-get install -y curl build-essential &&
    curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal &&
    . "$HOME/.cargo/env" && cargo build --release --locked'
  ```

The Docker command mounts both `my-site/` and its sibling `webnook/` so
the path dependency is available. Change the working directory if your
project has a different name. Use `linux/arm64` for an ARM64 server.

The result is `target/release/my-site`. Copy it to the server:

```sh
scp target/release/my-site you@example.com:/tmp/my-site
```

### 3. Set up the server

SSH in, then create a user for the site, its folders, and its settings
file:

```sh
sudo useradd --system --user-group --home /var/lib/my-site --shell /usr/sbin/nologin my-site
sudo install -d -m 0750 -o root -g my-site /etc/my-site /etc/my-site/tls
sudo install -d -m 0755 /var/lib/my-site/acme
sudo install -m 0755 /tmp/my-site /usr/local/bin/my-site

echo 'SITE_DOMAIN=example.com' | sudo tee /etc/my-site/env
sudo chmod 0640 /etc/my-site/env && sudo chgrp my-site /etc/my-site/env
```

Put any secrets (API keys and so on) in `/etc/my-site/env` too, never in
your code.

Open the firewall. Allow SSH **first** so you don't lock yourself out:

```sh
sudo ufw allow OpenSSH
sudo ufw allow 80,443/tcp
sudo ufw enable
```

### 4. Get a certificate

Install certbot the way its docs recommend:

```sh
sudo snap install --classic certbot
sudo ln -sf /snap/bin/certbot /usr/bin/certbot
```

The site isn't running yet, so port 80 is free and certbot can answer
Let's Encrypt's check by itself:

```sh
sudo certbot certonly --standalone --cert-name example.com -d example.com
```

### 5. Hand the certificate to the site

Certbot keeps its files readable by root only. A small "deploy hook" copies
them to `/etc/my-site/tls` with the right permissions, and reloads the site
after each renewal of this site's certificate. Copy
[`deploy/certbot-deploy-hook.sh`](deploy/certbot-deploy-hook.sh) from this
repository to the server. Edit its `EXPECTED_LINEAGE` to match the
certificate's directory (usually `/etc/letsencrypt/live/example.com`), and
replace `my-site` in its destination, group, and service name if needed.
The guard matters: Certbot runs directory hooks for other certificates
too. Skipped lineages are logged to stderr with the expected path, so a
misconfigured guard is visible in Certbot's logs. Install the hook and run
it once by hand:

```sh
sudo install -m 0755 certbot-deploy-hook.sh /etc/letsencrypt/renewal-hooks/deploy/my-site.sh
sudo RENEWED_LINEAGE=/etc/letsencrypt/live/example.com /etc/letsencrypt/renewal-hooks/deploy/my-site.sh
```

This first run copies the files without trying to reload a service that
has not been started yet.

### 6. Start the site with systemd

Copy [`deploy/my-site.service`](deploy/my-site.service) to the server. It
runs the site as the `my-site` user, lets it use ports 80 and 443 without
root, restarts it if it crashes, and locks down what it can touch.

```sh
sudo install -m 0644 my-site.service /etc/systemd/system/my-site.service
sudo systemctl daemon-reload
sudo systemctl enable --now my-site
```

Check that it works:

```sh
systemctl status my-site
curl -I https://example.com          # 200, with strict-transport-security
curl -I http://example.com           # 301 to https://example.com/
```

### 7. Switch renewals to the running site

Certbot's scheduled renewal keeps the certificate current. Now that the
site owns port 80, certbot can't use `--standalone` any more. Tell it to
drop its check files in the folder the site serves instead:

```sh
sudo certbot reconfigure --cert-name example.com --webroot -w /var/lib/my-site/acme
```

This tests domain validation and saves the webroot settings. When a real
renewal happens, the deploy hook copies the new files and sends the site a
reload signal: **the new certificate is picked up
without a restart and without dropping visitors.** Test validation,
certificate copying, and the reload hook together with:

```sh
sudo certbot renew --cert-name example.com --dry-run --run-deploy-hooks
```

`--run-deploy-hooks` is required to exercise the hook during a dry run.
Certbot passes the active certificate to the hook, so this test does not
install a staging certificate. See the [Certbot renewal guide](https://eff-certbot.readthedocs.io/en/stable/using.html#renewing-certificates).

### Everyday tasks

| Task | Command |
| --- | --- |
| Watch the logs | `journalctl -u my-site -f` |
| Deploy a new version | use the staged replacement below |
| Reload the certificate by hand | `sudo systemctl reload my-site` |
| Stop the site | `sudo systemctl stop my-site` (in-flight requests finish first) |

After copying the new binary to `/tmp/my-site`, replace the running
executable by renaming a staged file on the same filesystem:

```sh
sudo install -m 0755 /tmp/my-site /usr/local/bin/my-site.new
sudo mv /usr/local/bin/my-site.new /usr/local/bin/my-site
sudo systemctl restart my-site
curl -f https://example.com/health
```

### If something goes wrong

- **The site won't start**: `journalctl -u my-site -n 50` shows the error.
  `Bind { addr: "[::]:443", ... }` means something else is using the port;
  `Tls(...)` means the files in `/etc/my-site/tls` are missing or unreadable
  (rerun the hook from step 5).
- **certbot can't verify the domain**: make sure DNS points at the server,
  ports 80 and 443 are open in `ufw` and in your cloud provider's firewall,
  and `curl http://example.com/.well-known/acme-challenge/x` reaches the
  site (a `404` is fine; a timeout is not).
- **`Address family not supported`**: the server has no IPv6; bind
  `0.0.0.0` instead of `[::]` (see step 1).

## More deployment topics

[DEPLOYMENT.md](DEPLOYMENT.md) covers running behind Nginx, Caddy, or a load
balancer; backing up SQLite; a security checklist; and measuring memory use.

## Requirements

Rust 1.88 or newer.

## License

Licensed under the [MIT license](LICENSE).
