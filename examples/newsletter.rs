//! Newsletter signup through an outbound HTTP API, using the shared,
//! bounded [`webnook::HttpClient`].
//!
//! ```sh
//! cargo run --example newsletter --features http-client
//! curl -i -X POST http://127.0.0.1:3000/subscribe \
//!   -H 'Origin: http://127.0.0.1:3000' -d 'email=ada@example.com'
//! ```
//!
//! Set `NEWSLETTER_API_URL` and `NEWSLETTER_API_KEY` to call a real
//! provider. Without them the example posts to a stand-in provider served
//! by the same process, so it runs end to end offline.
//!
//! The API key comes from the environment, never from the binary or the
//! repository, and is not logged. Provider failures are logged by status
//! only and shown to visitors as a generic message.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use webnook::{HttpClient, HttpClientOptions, Server};

const ORIGIN: &str = "http://127.0.0.1:3000";

#[derive(Clone)]
struct AppState {
    client: HttpClient,
    api_url: Arc<str>,
    api_key: Arc<str>,
}

#[derive(Deserialize)]
struct Signup {
    email: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();

    let state = AppState {
        client: HttpClient::new(HttpClientOptions {
            request_timeout: Duration::from_secs(5),
            max_response_bytes: 64 * 1024,
            max_in_flight: 8,
            ..HttpClientOptions::default()
        })?,
        api_url: std::env::var("NEWSLETTER_API_URL")
            .unwrap_or_else(|_| format!("{ORIGIN}/stand-in-provider"))
            .into(),
        api_key: std::env::var("NEWSLETTER_API_KEY")
            .unwrap_or_else(|_| "development-key".into())
            .into(),
    };
    let app = Router::new()
        .route("/subscribe", post(subscribe))
        .with_state(state);

    Server::new()
        .router(app)
        .on("/stand-in-provider", post(stand_in_provider))
        .bind("127.0.0.1:3000")
        .serve()
        .await?;
    Ok(())
}

async fn subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(signup): Form<Signup>,
) -> Response {
    if headers.get(header::ORIGIN).map(|o| o.as_bytes()) != Some(ORIGIN.as_bytes()) {
        return (
            StatusCode::FORBIDDEN,
            "Cross-site submissions are not accepted.",
        )
            .into_response();
    }
    let email = signup.email.trim();
    if email.len() > 254 || !email.contains('@') {
        return (StatusCode::BAD_REQUEST, "Please check your email address.").into_response();
    }

    let request = state
        .client
        .post(&*state.api_url)
        .bearer_auth(&*state.api_key)
        .json(&json!({ "email": email }));
    match state
        .client
        .send(request)
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(_) => (StatusCode::OK, "Thanks for subscribing!").into_response(),
        Err(err) => {
            // `err` describes the status or transport failure, never the key.
            tracing::warn!(%err, "newsletter provider request failed");
            (
                StatusCode::BAD_GATEWAY,
                "We could not subscribe you right now; please try again later.",
            )
                .into_response()
        }
    }
}

/// Stands in for a provider API: accepts a bearer token and a JSON email.
async fn stand_in_provider(headers: HeaderMap, Json(body): Json<Value>) -> StatusCode {
    let authorized = headers
        .get(header::AUTHORIZATION)
        .is_some_and(|value| value.as_bytes().starts_with(b"Bearer "));
    match (authorized, body.get("email").and_then(Value::as_str)) {
        (true, Some(_)) => StatusCode::CREATED,
        (false, _) => StatusCode::UNAUTHORIZED,
        _ => StatusCode::UNPROCESSABLE_ENTITY,
    }
}
