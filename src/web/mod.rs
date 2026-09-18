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

#[cfg(test)]
mod tests {
    use super::Assets;

    fn asset(name: &str) -> String {
        let f = Assets::get(name).unwrap_or_else(|| panic!("asset {name} is missing"));
        String::from_utf8(f.data.into_owned()).expect("asset is not UTF-8")
    }

    /// Every legend swatch must name a class the stylesheet actually paints.
    ///
    /// The diff legend carried `d-3 d-2 d-1` while the stylesheet defined
    /// `.sw.d3 .sw.d2 .sw.d1`, so the three "shrank" chips rendered with no
    /// background whatsoever: the half of the scale that explains what blue
    /// means was simply invisible, and nothing anywhere failed to say so. A
    /// mistyped class name is precisely the kind of defect that reading the
    /// code does not catch, because both halves look right on their own.
    #[test]
    fn every_legend_swatch_is_painted_in_every_theme() {
        let html = asset("index.html");
        let css = asset("style.css");

        let steps: Vec<&str> = html
            .split("class=\"sw ")
            .skip(1)
            .map(|c| c.split('"').next().expect("unterminated class attribute"))
            .collect();
        assert_eq!(steps.len(), 7, "expected the seven diverging steps, found {steps:?}");

        for step in steps {
            assert!(
                css.contains(&format!(".sw.{step}{{")),
                "legend swatch `{step}` has no `.sw.{step}` rule, so it renders transparent"
            );
            // A glyph sits on each chip, so both the fill and its ink have to
            // be answered by every theme. Counting rather than merely finding
            // them catches the likelier mistake: adding a step to one theme
            // block and forgetting the other two.
            let fills = css.matches(&format!("--{step}:")).count();
            let inks = css.matches(&format!("--{step}-ink:")).count();
            assert!(fills > 0, "`--{step}` is never defined");
            assert_eq!(
                inks, fills,
                "`--{step}` is defined in {fills} theme(s) but `--{step}-ink` in {inks}: \
                 a theme paints this chip with no matching glyph colour"
            );
        }
    }
}
