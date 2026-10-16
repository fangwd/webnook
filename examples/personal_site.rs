//! A minimal personal-site example: a static directory, an embedded about
//! page, and two JSON endpoints.
//!
//! Run with `cargo run --example personal_site`, then open
//! <http://127.0.0.1:3000/>.

use axum::{
    response::Html,
    routing::{get, post},
    Json,
};
use serde_json::{json, Value};
use webnook::Server;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Server::new()
        .static_dir("./examples/assets/public")
        .on("/about", get(about))
        .on("/api/foo", get(foo))
        .on("/api/echo", post(echo))
        .bind("127.0.0.1:3000")
        .serve()
        .await?;

    Ok(())
}

async fn about() -> Html<&'static str> {
    Html(include_str!("./assets/pages/about.html"))
}

async fn foo() -> Json<Value> {
    Json(json!({ "message": "Hello from Webnook!" }))
}

async fn echo(Json(body): Json<Value>) -> Json<Value> {
    Json(body)
}
