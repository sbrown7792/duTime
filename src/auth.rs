//! Bearer-token access control.
//!
//! One shared secret, checked on every data endpoint. Deliberately not a user
//! and password system: accounts mean a user table, password hashing, session
//! cookies, CSRF defence and a reset flow, which is a large amount of
//! security-sensitive surface to maintain for a single-operator diagnostic
//! tool. A bearer token is one comparison, it cannot be replayed from a
//! cross-site form the way a cookie can, and it is what the `curl` in your
//! shell history wants anyway.
//!
//! What is protected: every endpoint that returns data about the filesystem.
//! What is not: the static UI, and `/api/v1/health`. The UI shell has to load
//! before it can ask for a token, and it contains no data — the filenames and
//! sizes all arrive over the API it cannot call yet. `health` reports
//! liveness, a version and a count of roots, which is what an uptime check
//! needs and reveals nothing about the disk.

use std::path::Path;

#[derive(Clone, Debug)]
pub enum Auth {
    /// No token configured. Anyone who can reach the port sees everything.
    Open,
    Token(String),
}

impl Auth {
    pub fn from_config(cfg: &crate::config::AuthConfig) -> anyhow::Result<Self> {
        if let Ok(t) = std::env::var("DUTIME_TOKEN") {
            let t = t.trim().to_string();
            if !t.is_empty() {
                return Ok(Auth::Token(t));
            }
        }
        let file = std::env::var("DUTIME_TOKEN_FILE")
            .ok()
            .map(std::path::PathBuf::from)
            .or_else(|| cfg.token_file.clone());
        if let Some(p) = file {
            return Ok(Auth::Token(read_token(&p)?));
        }
        if let Some(t) = cfg.token.as_ref().map(|t| t.trim()).filter(|t| !t.is_empty()) {
            return Ok(Auth::Token(t.to_string()));
        }
        Ok(Auth::Open)
    }

    pub fn is_open(&self) -> bool {
        matches!(self, Auth::Open)
    }

    /// Does this `Authorization` header carry the right token?
    pub fn allows(&self, header: Option<&str>) -> bool {
        let Auth::Token(want) = self else { return true };
        let Some(h) = header else { return false };
        // Accept "Bearer <t>" and a bare token, since half the world's
        // scripts send one and half the other.
        let got = h
            .strip_prefix("Bearer ")
            .or_else(|| h.strip_prefix("bearer "))
            .unwrap_or(h)
            .trim();
        ct_eq(got.as_bytes(), want.as_bytes())
    }
}

/// Read a token from a file, refusing the ways it is usually got wrong.
fn read_token(p: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(p)
        .map_err(|e| anyhow::anyhow!("reading auth.token_file {}: {e}", p.display()))?;
    let t = raw.trim().to_string();
    if t.is_empty() {
        anyhow::bail!(
            "auth.token_file {} is empty — that would leave the UI open while looking \
             protected. Generate one with `dutime token`.",
            p.display()
        );
    }
    // A short token is worse than none, because it looks like security.
    if t.len() < 16 {
        anyhow::bail!(
            "the token in {} is {} characters; at least 16 are required. \
             `dutime token` writes a 256-bit one.",
            p.display(),
            t.len()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(m) = std::fs::metadata(p)
            && m.mode() & 0o044 != 0
        {
            tracing::warn!(
                "{} is readable by other users (mode {:o}); run: chmod 600 {}",
                p.display(),
                m.mode() & 0o777,
                p.display()
            );
        }
    }
    Ok(t)
}

/// Compare without leaking *where* two tokens first differ.
///
/// A byte-by-byte `==` returns as soon as it finds a mismatch, so the time it
/// takes reveals how much of a guess was correct, and a few thousand requests
/// recover the secret one byte at a time. This reads both buffers to the end
/// every time.
///
/// The length is not hidden. That is deliberate and harmless: the length of a
/// `dutime token` is a documented constant, not a secret.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

/// A fresh 256-bit token, URL-safe.
///
/// Straight from the kernel CSPRNG. No dependency, and nothing in between to
/// get wrong — the commonest way a token generator fails is by seeding a
/// general-purpose PRNG from the clock.
pub fn generate() -> anyhow::Result<String> {
    use base64::Engine;
    let mut buf = [0u8; 32];
    let mut f = std::fs::File::open("/dev/urandom")?;
    std::io::Read::read_exact(&mut f, &mut buf)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_allows_anything() {
        let a = Auth::Open;
        assert!(a.allows(None));
        assert!(a.allows(Some("Bearer nonsense")));
    }

    #[test]
    fn a_token_is_required_and_must_match() {
        let a = Auth::Token("s3cret-token-long-enough".into());
        assert!(!a.allows(None), "a missing header was accepted");
        assert!(!a.allows(Some("")));
        assert!(!a.allows(Some("Bearer wrong")));
        assert!(a.allows(Some("Bearer s3cret-token-long-enough")));
        assert!(a.allows(Some("bearer s3cret-token-long-enough")));
        // A bare token, as scripts tend to send.
        assert!(a.allows(Some("s3cret-token-long-enough")));
    }

    /// A prefix of the real token must not be accepted, which is the bug a
    /// hand-rolled comparison invites.
    #[test]
    fn a_prefix_is_not_enough() {
        let a = Auth::Token("abcdefghijklmnopqrst".into());
        assert!(!a.allows(Some("Bearer abcdefghij")));
        assert!(!a.allows(Some("Bearer abcdefghijklmnopqrstuvwxyz")));
    }

    #[test]
    fn generated_tokens_are_long_and_distinct() {
        let a = generate().unwrap();
        let b = generate().unwrap();
        assert_ne!(a, b, "two generated tokens were identical");
        assert_eq!(a.len(), 43, "expected 256 bits base64url-encoded");
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn an_empty_token_file_is_refused_rather_than_ignored() {
        let p = std::env::temp_dir().join("dutime-auth-empty");
        std::fs::write(&p, "   \n").unwrap();
        let e = read_token(&p).unwrap_err().to_string();
        assert!(e.contains("empty"), "{e}");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_short_token_is_refused() {
        let p = std::env::temp_dir().join("dutime-auth-short");
        std::fs::write(&p, "hunter2").unwrap();
        let e = read_token(&p).unwrap_err().to_string();
        assert!(e.contains("16"), "{e}");
        let _ = std::fs::remove_file(&p);
    }
}

// ── request plumbing ─────────────────────────────────────────────────────

/// Whether the current request presented a valid token.
///
/// A separate type rather than a bare `bool` so it cannot be confused with
/// any other flag at a call site, and so every handler that makes an
/// authorization decision names it in its signature. An ambient value read
/// from a thread-local would be shorter and is precisely how this kind of
/// check ends up silently skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Viewer {
    Anonymous,
    Authenticated,
}

impl Viewer {
    pub fn authed(self) -> bool {
        self == Viewer::Authenticated
    }
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Viewer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _: &S,
    ) -> Result<Self, Self::Rejection> {
        // Fails closed. If the middleware is ever missing from a route, the
        // request is treated as anonymous, which can only restrict.
        Ok(parts.extensions.get::<Viewer>().copied().unwrap_or(Viewer::Anonymous))
    }
}

/// Classify every request, and reject the ones asking for a protected root.
///
/// Two jobs, because they need the same header parse. Classification is
/// recorded for the handlers; rejection happens here only for the
/// unambiguous case — an explicit `?root=` naming a protected root. Requests
/// with no `root` are left alone, because the default root is resolved from
/// what the caller may actually see, so an anonymous visitor lands on a
/// visible root rather than a 401.
pub async fn middleware(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::api::AppState>>,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let header = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let viewer = if state.auth.allows(header) { Viewer::Authenticated } else { Viewer::Anonymous };
    req.extensions_mut().insert(viewer);

    if !viewer.authed()
        && let Some(id) = explicit_root(req.uri().query())
        && state.is_protected(id)
    {
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            [(axum::http::header::WWW_AUTHENTICATE, "Bearer")],
            axum::Json(serde_json::json!({
                "error": "this root is protected; sign in to view it",
                "protected": true,
            })),
        )
            .into_response();
    }
    next.run(req).await
}

/// The `root` query parameter, if the caller named one.
fn explicit_root(query: Option<&str>) -> Option<crate::model::RootId> {
    query?
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .find(|(k, _)| *k == "root")
        .and_then(|(_, v)| v.parse().ok())
}

#[cfg(test)]
mod request_tests {
    use super::*;

    #[test]
    fn finds_the_root_parameter_wherever_it_sits() {
        assert_eq!(explicit_root(Some("root=3")), Some(3));
        assert_eq!(explicit_root(Some("at=now&root=2&metric=apparent")), Some(2));
        assert_eq!(explicit_root(Some("metric=apparent")), None);
        assert_eq!(explicit_root(None), None);
        // Must not be fooled by a parameter that merely ends in "root".
        assert_eq!(explicit_root(Some("subroot=7")), None);
        assert_eq!(explicit_root(Some("root=notanumber")), None);
    }
}
