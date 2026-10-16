//! A single-binary example: the `public/` folder is embedded with
//! `rust-embed`, so the release binary needs no files on disk.
//!
//! ```sh
//! cargo run --example embedded --features embed
//! ```
//!
//! Then open <http://127.0.0.1:3000/> and <http://127.0.0.1:3000/about>.

use axum::routing::get;
use rust_embed::RustEmbed;
use webnook::{Asset, Assets, Cache, Server};

#[derive(RustEmbed)]
#[folder = "examples/assets/public/"]
struct Public;

#[tokio::main(flavor = "current_thread")]
async fn main() -> webnook::Result<()> {
    let assets = Assets::new()
        .embed::<Public>()
        .add("/about.html", include_str!("assets/pages/about.html"))
        // Content-hashed file names can be cached forever.
        .add(
            "/logo-d41d8cd9.svg",
            Asset::new(r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#).cache(Cache::Immutable),
        );

    Server::new()
        .assets(assets)
        .not_found(include_str!("assets/public/404.html"))
        .on("/health", get(|| async { "ok" }))
        .bind("127.0.0.1:3000")
        .serve()
        .await
}
