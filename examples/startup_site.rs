//! A startup-site example: embedded pages plus a contact form stored in
//! SQLite, with the protections a public form needs.
//!
//! ```sh
//! cargo run --example startup_site --features sqlite
//! ```
//!
//! Open <http://127.0.0.1:3000/contact> and submit the form. Configuration
//! comes from the environment:
//!
//! - `SITE_ORIGIN`: the origin browsers submit from
//!   (default `http://127.0.0.1:3000`)
//! - `DATABASE_PATH`: SQLite file (default `./site.db`)
//! - `TRUST_PROXY`: proxy range to trust for client addresses, if behind one
//!
//! Patterns shown:
//!
//! - **CSRF**: browsers send `Origin` on form posts; only the site's own
//!   origin is accepted. CORS does not protect simple form posts.
//! - **Abuse controls**: per-address rate limit with bounded memory, a body
//!   limit for this route, and a honeypot field that bots tend to fill.
//! - **Safe errors**: validation errors are generic; database errors are
//!   logged without personal data and never shown to visitors.
//! - **Post/redirect/get**: success redirects to a thank-you page.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::get;
use axum::{Form, Router};
use serde::Deserialize;
use webnook::{Assets, ClientIp, Server, Sqlite, SqliteOptions};

const CONTACT_PAGE: &str = r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8" /><title>Contact</title><link rel="stylesheet" href="/site.css" /></head>
  <body><main>
    <h1>Contact us</h1>
    <form method="post" action="/contact">
      <label>Email <input type="email" name="email" required maxlength="254" /></label>
      <label>Message <textarea name="message" required maxlength="5000"></textarea></label>
      <input type="text" name="website" tabindex="-1" autocomplete="off" hidden />
      <button type="submit">Send</button>
    </form>
  </main></body>
</html>"#;

const THANKS_PAGE: &str = r#"<!doctype html>
<html lang="en">
  <head><meta charset="utf-8" /><title>Thanks</title><link rel="stylesheet" href="/site.css" /></head>
  <body><main><h1>Thanks!</h1><p>We will get back to you soon.</p></main></body>
</html>"#;

#[derive(Clone)]
struct AppState {
    db: Sqlite,
    origin: Arc<str>,
    limiter: Arc<RateLimiter>,
}

#[derive(Deserialize)]
struct Contact {
    email: String,
    message: String,
    /// Honeypot: hidden from people, often filled in by bots.
    #[serde(default)]
    website: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    let origin = env_or("SITE_ORIGIN", "http://127.0.0.1:3000");
    let db = Sqlite::open(
        env_or("DATABASE_PATH", "./site.db"),
        SqliteOptions::default(),
    )?;
    db.migrate(&["CREATE TABLE contacts (
            id INTEGER PRIMARY KEY,
            email TEXT NOT NULL,
            message TEXT NOT NULL,
            created_at TEXT NOT NULL DEFAULT (datetime('now'))
        );"])
        .await?;

    let state = AppState {
        db,
        origin: origin.into(),
        limiter: Arc::new(RateLimiter::new(5, Duration::from_secs(600), 10_000)),
    };
    let contact = Router::new()
        .route(
            "/contact",
            get(|| async { Html(CONTACT_PAGE) })
                .post(submit_contact)
                .layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .with_state(state);

    let assets = Assets::new()
        .add("/index.html", include_str!("./assets/public/index.html"))
        .add("/site.css", include_str!("./assets/public/site.css"))
        .add("/thanks.html", THANKS_PAGE);

    let mut server = Server::new()
        .assets(assets)
        .not_found(include_str!("./assets/public/404.html"))
        .router(contact)
        .on("/health", get(|| async { "ok" }))
        .bind("127.0.0.1:3000");
    if let Ok(range) = std::env::var("TRUST_PROXY") {
        server = server.trust_proxy(&range);
    }
    server.serve().await?;
    Ok(())
}

async fn submit_contact(
    State(state): State<AppState>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    Form(contact): Form<Contact>,
) -> Response {
    if !same_origin(&headers, &state.origin) {
        return (
            StatusCode::FORBIDDEN,
            "Cross-site submissions are not accepted.",
        )
            .into_response();
    }
    if !state.limiter.allow(ip) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Too many messages; try again later.",
        )
            .into_response();
    }
    if !contact.website.is_empty() {
        // Pretend success so bots learn nothing.
        return Redirect::to("/thanks").into_response();
    }
    let email = contact.email.trim().to_string();
    let message = contact.message.trim().to_string();
    let valid_email = email.len() <= 254
        && email
            .split_once('@')
            .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'));
    if !valid_email || message.is_empty() || message.chars().count() > 5000 {
        return (
            StatusCode::BAD_REQUEST,
            "Please check your email and message.",
        )
            .into_response();
    }

    let saved = state
        .db
        .call(move |conn| {
            conn.execute(
                "INSERT INTO contacts (email, message) VALUES (?1, ?2)",
                (&email, &message),
            )
        })
        .await;
    match saved {
        Ok(_) => Redirect::to("/thanks").into_response(),
        Err(err) if err.is_busy() => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "5")],
            "We are busy right now; please try again shortly.",
        )
            .into_response(),
        Err(err) => {
            // The error carries no visitor data; the visitor sees no details.
            tracing::error!(%err, "failed to store contact message");
            (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong.").into_response()
        }
    }
}

/// Browsers send `Origin` with form posts; accept only our own.
fn same_origin(headers: &HeaderMap, origin: &str) -> bool {
    headers
        .get(header::ORIGIN)
        .is_some_and(|value| value.as_bytes() == origin.as_bytes())
}

/// Fixed-window limit per address. Memory is bounded by `capacity`; when
/// full of active windows, new addresses are refused (fail closed).
struct RateLimiter {
    limit: u32,
    window: Duration,
    capacity: usize,
    hits: Mutex<HashMap<IpAddr, (Instant, u32)>>,
}

impl RateLimiter {
    fn new(limit: u32, window: Duration, capacity: usize) -> Self {
        Self {
            limit,
            window,
            capacity,
            hits: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut hits = self
            .hits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !hits.contains_key(&ip) && hits.len() >= self.capacity {
            hits.retain(|_, (start, _)| now.duration_since(*start) < self.window);
            if hits.len() >= self.capacity {
                return false;
            }
        }
        let (start, count) = hits.entry(ip).or_insert((now, 0));
        if now.duration_since(*start) >= self.window {
            *start = now;
            *count = 0;
        }
        *count += 1;
        *count <= self.limit
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}
