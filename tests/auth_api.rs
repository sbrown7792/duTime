//! Per-root authorization.
//!
//! The promise is specific: a caller with no token sees the unprotected roots
//! and is not told the protected ones exist, while the same caller with the
//! token sees everything. Both halves are load-bearing — a gate that also
//! blocks the public roots is useless, and one that lists a protected root it
//! will not open has already leaked the path.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dutime::api::{AppState, router};
use dutime::auth::Auth;
use dutime::scan::walker::{ScanOptions, scan};
use dutime::store::Store;
use dutime::store::commit::{CommitOptions, commit_scan};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "test-token-long-enough-to-pass";

/// Two roots in one store: one public, one protected.
fn fixture() -> (Arc<AppState>, tempfile::TempDir, i64, i64) {
    fs::create_dir_all("target/fixtures").unwrap();
    let dir = tempfile::Builder::new()
        .prefix("dutime-auth-")
        .tempdir_in("target/fixtures")
        .unwrap();
    let base = dir.path().canonicalize().unwrap();

    let mut store = Store::open_in_memory().unwrap();
    let mut ids = Vec::new();
    let mut clock = dutime::cli::now() - 86_400;

    for (name, file) in [("pub", "shared/report.bin"), ("priv", "data/holiday-photos.bin")] {
        let root = base.join(name);
        let p = root.join(file);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::File::create(&p).unwrap().write_all(&vec![b'x'; 3 << 20]).unwrap();

        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        clock += 3600;
        let id = store.ensure_root(&root).unwrap();
        commit_scan(
            &mut store, id, &root, &r.tree, &roll, &r.stats, clock, 10,
            &CommitOptions { checkpoint_every_scans: 4, checkpoint_min_bytes: 0 },
        )
        .unwrap();
        ids.push(id);
    }

    let mut state = AppState::new(store);
    state.set_access(Auth::Token(TOKEN.into()), HashSet::from([ids[1]]));
    (Arc::new(state), dir, ids[0], ids[1])
}

/// `token: None` is an anonymous caller.
async fn req(state: &Arc<AppState>, uri: &str, token: Option<&str>) -> (StatusCode, Value) {
    let mut b = Request::builder().uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    let res =
        router(state.clone()).oneshot(b.body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20).await.unwrap();
    let v = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("non-JSON from {uri}: {}", String::from_utf8_lossy(&bytes)));
    (status, v)
}

fn root_paths(v: &Value) -> Vec<String> {
    v["roots"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["path"]["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_protected_root_is_not_listed_to_anonymous_callers() {
    let (state, _d, _pub_id, _priv_id) = fixture();

    let (st, v) = req(&state, "/api/v1/roots", None).await;
    assert_eq!(st, StatusCode::OK, "listing roots must work without a token");
    let paths = root_paths(&v);
    assert_eq!(paths.len(), 1, "expected only the public root, got {paths:?}");
    assert!(paths[0].ends_with("/pub"));
    // The path itself is information; it must not appear at all.
    assert!(!v.to_string().contains("/priv"), "the protected path leaked: {v}");

    let (st, v) = req(&state, "/api/v1/roots", Some(TOKEN)).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(root_paths(&v).len(), 2, "the token did not reveal both roots: {v}");
}

/// Every data endpoint, not just the one that was easiest to remember.
#[tokio::test]
async fn every_endpoint_refuses_a_protected_root_without_the_token() {
    let (state, _d, pub_id, priv_id) = fixture();

    // Endpoint, plus whatever parameters it insists on.
    let eps = [
        ("overview", ""),
        ("tree", ""),
        ("listing", ""),
        ("series", ""),
        ("gainers", "&from=-7d"),
        ("scans", ""),
        ("resolve", "&at=now"),
        ("diff", "&from=now&to=now"),
    ];
    for (ep, extra) in eps {
        // Note the refusal lands before the query string is even parsed, so
        // it does not depend on the rest of the request being well formed.
        let (st, v) = req(&state, &format!("/api/v1/{ep}?root={priv_id}{extra}"), None).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{ep} allowed an anonymous caller: {v}");
        assert!(!v.to_string().contains("holiday-photos"), "{ep} leaked a filename: {v}");

        // The same endpoint on the public root must still work, or the gate
        // has broken the thing it was supposed to leave alone.
        let (st, v) = req(&state, &format!("/api/v1/{ep}?root={pub_id}{extra}"), None).await;
        assert_eq!(st, StatusCode::OK, "{ep} broke for the public root: {v}");

        // And with the token, the protected root opens.
        let (st, v) =
            req(&state, &format!("/api/v1/{ep}?root={priv_id}{extra}"), Some(TOKEN)).await;
        assert_eq!(st, StatusCode::OK, "{ep} refused a valid token: {v}");
    }
}

/// A 401 must be answerable, so it has to say that a token would help.
#[tokio::test]
async fn the_refusal_says_how_to_fix_it() {
    let (state, _d, _pub_id, priv_id) = fixture();
    let (st, v) = req(&state, &format!("/api/v1/overview?root={priv_id}"), None).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["protected"], serde_json::json!(true));
    assert!(v["error"].as_str().unwrap().contains("sign in"), "{v}");
}

/// A request that names no root must land on one the caller may see, rather
/// than failing because the first root happens to be protected.
#[tokio::test]
async fn the_default_root_is_one_the_caller_can_actually_see() {
    let (state, _d, pub_id, priv_id) = fixture();

    let (st, v) = req(&state, "/api/v1/overview", None).await;
    assert_eq!(st, StatusCode::OK, "an anonymous caller got no default root: {v}");
    assert_eq!(v["root_id"], serde_json::json!(pub_id));

    // Signed in, the default is the first root outright.
    let (_, v) = req(&state, "/api/v1/overview", Some(TOKEN)).await;
    assert_eq!(v["root_id"], serde_json::json!(pub_id));
    let (st, _) = req(&state, &format!("/api/v1/overview?root={priv_id}"), Some(TOKEN)).await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn a_wrong_token_is_exactly_as_good_as_none() {
    let (state, _d, _pub_id, priv_id) = fixture();
    for bad in ["", "wrong", "test-token-long-enough-to-pas", "test-token-long-enough-to-passX"] {
        let (st, _) = req(&state, &format!("/api/v1/listing?root={priv_id}"), Some(bad)).await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "token {bad:?} was accepted");
    }
}

/// Health has to answer without a token or an uptime check cannot use it,
/// so it must not carry anything worth protecting.
#[tokio::test]
async fn health_is_open_but_says_nothing_about_the_disk() {
    let (state, _d, _pub_id, _priv_id) = fixture();
    let (st, v) = req(&state, "/api/v1/health", None).await;
    assert_eq!(st, StatusCode::OK);
    let s = v.to_string();
    assert!(!s.contains("/priv") && !s.contains("holiday"), "health leaked a path: {v}");
    // A count of roots is fine; the paths are not.
    assert!(v["roots"].is_number());
}

#[tokio::test]
async fn auth_status_reports_what_the_ui_needs_and_no_more() {
    let (state, _d, _pub_id, _priv_id) = fixture();

    let (_, v) = req(&state, "/api/v1/auth", None).await;
    assert_eq!(v["required"], serde_json::json!(true));
    assert_eq!(v["authenticated"], serde_json::json!(false));

    let (_, v) = req(&state, "/api/v1/auth", Some(TOKEN)).await;
    assert_eq!(v["authenticated"], serde_json::json!(true));

    // With nothing gated there is nothing to offer, and the UI hides the
    // control rather than posing a question with no useful answer.
    let store = Store::open_in_memory().unwrap();
    let mut open = AppState::new(store);
    open.set_access(Auth::Open, HashSet::new());
    let (_, v) = req(&Arc::new(open), "/api/v1/auth", None).await;
    assert_eq!(v["required"], serde_json::json!(false));
}
