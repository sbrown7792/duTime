//! Static asset serving.
//!
//! Assets are compiled into the binary, so deployment is one file with no
//! asset path to get wrong and nothing to go missing on upgrade. `debug-embed`
//! is deliberately off, so a debug build reads `web/` from disk and editing the
//! UI does not mean recompiling Rust.

use axum::http::{StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "web/"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };

    match Assets::get(path) {
        Some(f) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            // Vendored libraries are pinned by filename and never change in
            // place; everything else must be revalidated so a UI fix is not
            // stuck behind a stale cache.
            let cache = if path.starts_with("vendor/") {
                "public, max-age=31536000, immutable"
            } else {
                "no-cache"
            };
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref()),
                    (header::CACHE_CONTROL, cache),
                ],
                f.data,
            )
                .into_response()
        }
        // Unknown paths fall through to the app shell so client-side routes
        // survive a reload, but a missing API call still 404s honestly.
        None if !path.starts_with("api/") => match Assets::get("index.html") {
            Some(f) => ([(header::CONTENT_TYPE, "text/html")], f.data).into_response(),
            None => (StatusCode::NOT_FOUND, "not found").into_response(),
        },
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}
