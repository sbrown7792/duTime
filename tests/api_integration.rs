//! End-to-end tests against the real axum handlers.
//!
//! These exist because two genuine bugs got through the store-level tests and
//! were only caught by looking at rendered output:
//!
//!  * the stacked-area feed dropped events belonging to paths deleted during
//!    the window, so a band kept bytes that no longer existed;
//!  * the diff treemap sized tiles by their size at the end of the window, so
//!    anything deleted had zero area and disappeared from the picture — losing
//!    exactly the half of the story that `du` can never tell you.
//!
//! Both are invisible unless you check the totals, so both are checked here.

use axum::body::Body;
use axum::http::Request;
use dutime::api::{AppState, router};
use dutime::scan::walker::{ScanOptions, scan};
use dutime::store::Store;
use dutime::store::commit::{CommitOptions, commit_scan};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    clock: i64,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        fs::create_dir_all("target/fixtures").unwrap();
        let dir = tempfile::Builder::new()
            .prefix("dutime-api-")
            .tempdir_in("target/fixtures")
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = Store::open_in_memory().unwrap();
        store.ensure_root(&root).unwrap();
        Self {
            _dir: dir,
            root,
            // Anchor to real time: relative windows like "-365d" are resolved
            // against the wall clock inside the handlers, so a fixture stuck
            // in 2023 would make every window look fully covered.
            clock: dutime::cli::now() - 86_400,
            store,
        }
    }

    fn write(&self, rel: &str, n: usize) {
        let p = self.root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::File::create(&p).unwrap().write_all(&vec![b'x'; n]).unwrap();
    }

    fn snapshot(&mut self) {
        let mut o = ScanOptions::new(&self.root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        self.clock += 3600;
        let root_id = self.store.ensure_root(&self.root).unwrap();
        commit_scan(
            &mut self.store, root_id, &self.root, &r.tree, &roll, &r.stats,
            self.clock, 10, &CommitOptions { checkpoint_every_scans: 4, checkpoint_min_bytes: 0 },
        )
        .unwrap();
    }

    /// Move the populated store into the server state and build the router.
    fn finish(self) -> (Arc<AppState>, std::path::PathBuf) {
        let state = Arc::new(AppState::new(self.store));
        (state, self.root)
    }
}

async fn get(state: &Arc<AppState>, uri: &str) -> Value {
    let app = router(state.clone());
    let res = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 8 << 20).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("non-JSON response: {}", String::from_utf8_lossy(&bytes)));
    assert!(status.is_success(), "GET {uri} -> {status}: {v}");
    v
}

fn enc(p: &std::path::Path) -> String {
    // Percent-encode just enough for a query string.
    p.display().to_string().replace('%', "%25").replace('&', "%26").replace('+', "%2B")
}

/// Every sample of the stacked area must sum to the tree's recorded total.
///
/// The regression: a directory deleted mid-window emitted its large negative
/// event and then vanished from the end-of-window snapshot, so the attribution
/// walk could not find which band it belonged to and skipped it. The band kept
/// the bytes forever and the stack floated above the real total.
#[tokio::test]
async fn stacked_area_sums_to_the_tree_total_at_every_sample() {
    let mut f = Fixture::new();
    f.write("keep/a.bin", 4 << 20);
    f.write("doomed/big1.bin", 20 << 20);
    f.write("doomed/nested/big2.bin", 30 << 20);
    f.snapshot();
    f.write("keep/b.bin", 8 << 20);
    f.snapshot();
    // Delete a whole subtree mid-window — the case that broke.
    fs::remove_dir_all(f.root.join("doomed")).unwrap();
    f.snapshot();
    f.write("keep/c.bin", 2 << 20);
    f.snapshot();

    let (state, root) = f.finish();
    let series = get(&state, &format!("/api/v1/series?path={}&from=-7d&children=8", enc(&root))).await;
    let scans = get(&state, "/api/v1/scans?limit=500").await;

    let totals: std::collections::HashMap<i64, i64> = scans["scans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["at"].as_i64().unwrap(), s["bytes"].as_i64().unwrap()))
        .collect();

    let times = series["times"].as_array().unwrap();
    let bands = series["bands"].as_array().unwrap();
    assert!(!times.is_empty(), "expected samples");

    for (i, t) in times.iter().enumerate() {
        let t = t.as_i64().unwrap();
        let stacked: i64 = bands
            .iter()
            .map(|b| b["points"][i].as_i64().unwrap())
            .sum();
        let truth = totals[&t];
        assert_eq!(
            stacked, truth,
            "stack != recorded total at sample {i} (t={t}); a deleted subtree's \
             negative delta was probably dropped"
        );
    }
}

/// A deleted directory must still appear in the diff treemap.
///
/// Sizing tiles by their size at the end of the window gives anything deleted
/// zero area, so it silently disappears — losing the "where did 700 MB go?"
/// half of the comparison. Tiles are sized by max(before, after) instead.
#[tokio::test]
async fn diff_treemap_keeps_deleted_directories_visible() {
    let mut f = Fixture::new();
    f.write("stays/a.bin", 4 << 20);
    f.write("vanishes/huge.bin", 64 << 20);
    f.snapshot();
    fs::remove_dir_all(f.root.join("vanishes")).unwrap();
    f.write("stays/b.bin", 8 << 20);
    f.snapshot();

    let (state, _root) = f.finish();
    let scans = get(&state, "/api/v1/scans?limit=500").await;
    let ids: Vec<i64> = scans["scans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["scan_id"].as_i64().unwrap())
        .collect();
    let (first, last) = (ids[0], *ids.last().unwrap());

    let d = get(&state, &format!("/api/v1/diff?from=scan:{first}&to=scan:{last}&depth=3")).await;
    let kids = d["node"]["children"].as_array().expect("children");

    let gone = kids
        .iter()
        .find(|c| c["name"] == "vanishes")
        .expect("the deleted directory must still be present in the diff");
    assert_eq!(gone["gone"], true, "it should be flagged as deleted");
    assert_eq!(gone["after"].as_i64().unwrap(), 0);
    assert!(gone["before"].as_i64().unwrap() >= 64 << 20);
    assert!(
        gone["value"].as_i64().unwrap() >= 64 << 20,
        "its tile must keep the area it used to occupy, not collapse to zero"
    );
    assert!(gone["delta"].as_i64().unwrap() <= -(64 << 20));

    let grew = kids.iter().find(|c| c["name"] == "stays").unwrap();
    assert!(grew["delta"].as_i64().unwrap() > 0);
}

/// Treemap children plus the synthesized extras must account for the parent.
///
/// A treemap whose rectangles do not add up to their container silently
/// misattributes space, which is worse than showing nothing.
#[tokio::test]
async fn treemap_children_account_for_the_whole_parent() {
    let mut f = Fixture::new();
    for i in 0..12 {
        f.write(&format!("d{i}/f.bin"), (2 << 20) + i * 4096);
    }
    // Loose files directly in the root, below the tracking threshold.
    f.write("loose1.txt", 900);
    f.write("loose2.txt", 1200);
    f.snapshot();

    let (state, root) = f.finish();
    // Ask for fewer children than exist, to force both synthesized nodes.
    let t = get(&state, &format!("/api/v1/tree?path={}&depth=1&limit=5", enc(&root))).await;
    let total = t["total"].as_i64().unwrap();
    let sum: i64 = t["node"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["value"].as_i64().unwrap())
        .sum();
    assert_eq!(sum, total, "children must sum to the parent; space went missing");

    let names: Vec<&str> = t["node"]["children"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(names.iter().any(|n| n.contains("more")), "expected an 'N more' node: {names:?}");
    assert!(names.iter().any(|n| n.contains("files here")), "expected a 'files here' node: {names:?}");
}

/// A window reaching past the first scan must not report the baseline as growth.
#[tokio::test]
async fn gainers_window_clamps_to_the_first_scan() {
    let mut f = Fixture::new();
    f.write("a/big.bin", 50 << 20);
    f.snapshot();
    f.write("a/extra.bin", 3 << 20);
    f.snapshot();

    let (state, _) = f.finish();
    let g = get(&state, "/api/v1/gainers?from=-365d&limit=20").await;
    assert_eq!(g["window_clamped_to_first_scan"], true);

    let results = g["results"].as_array().unwrap();
    let biggest = results.first().map(|r| r["delta"].as_i64().unwrap()).unwrap_or(0);
    assert!(
        biggest < 50 << 20,
        "the baseline's births leaked into the window: reported {biggest} bytes of growth"
    );
}

/// Filenames that are not valid UTF-8 must survive the JSON boundary.
#[tokio::test]
async fn non_utf8_names_round_trip_through_json() {
    use std::os::unix::ffi::OsStrExt;
    let mut f = Fixture::new();
    let weird = std::ffi::OsStr::from_bytes(b"weird-\xff\xfe-dir");
    fs::create_dir_all(f.root.join(weird)).unwrap();
    f.write(&format!("{}/inside.bin", weird.to_string_lossy()), 1);
    // The lossy join above will not have created the real name, so make it here.
    let real = f.root.join(weird);
    fs::create_dir_all(&real).unwrap();
    fs::File::create(real.join("blob.bin")).unwrap().write_all(&vec![b'x'; 2 << 20]).unwrap();
    f.snapshot();

    let (state, root) = f.finish();
    let t = get(&state, &format!("/api/v1/tree?path={}&depth=1&limit=50", enc(&root))).await;
    let kids = t["node"]["children"].as_array().unwrap();
    let odd = kids
        .iter()
        .find(|c| c["lossy"] == true)
        .expect("the non-UTF-8 directory should be reported as lossy");
    assert!(
        odd["name_b64"].is_string(),
        "a lossy name must carry the exact bytes in base64, or the caller can never recover it"
    );
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(odd["name_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(raw, b"weird-\xff\xfe-dir", "the exact filename bytes must survive");
}

/// Each row's sparkline must end at the size the row reports.
///
/// The sparkline is replayed forward from the window's start while the size
/// comes from the end-of-window snapshot. If those two disagree the trend is
/// decorative rather than true, which is worse than having no trend at all.
#[tokio::test]
async fn listing_sparklines_end_at_the_reported_size() {
    let mut f = Fixture::new();
    f.write("alpha/a.bin", 4 << 20);
    f.write("beta/b.bin", 9 << 20);
    f.write("gamma/c.bin", 2 << 20);
    f.snapshot();
    f.write("alpha/a.bin", 30 << 20);
    f.snapshot();
    f.write("beta/extra.bin", 5 << 20);
    f.snapshot();
    fs::remove_dir_all(f.root.join("gamma")).unwrap();
    f.write("alpha/a.bin", 12 << 20);
    f.snapshot();

    let (state, root) = f.finish();
    let l = get(&state, &format!("/api/v1/listing?path={}&from=-7d&points=16", enc(&root))).await;

    let rows = l["rows"].as_array().unwrap();
    assert!(!rows.is_empty());
    for r in rows {
        let spark = r["spark"].as_array().unwrap();
        assert!(!spark.is_empty(), "{} has no sparkline", r["name"]);
        assert_eq!(
            spark.last().unwrap().as_i64().unwrap(),
            r["size"].as_i64().unwrap(),
            "sparkline for {} does not end at its reported size",
            r["name"]
        );
        assert_eq!(
            r["delta"].as_i64().unwrap(),
            r["size"].as_i64().unwrap() - r["before"].as_i64().unwrap()
        );
    }
}

/// Rows plus the directory's own files must account for the whole directory.
#[tokio::test]
async fn listing_rows_account_for_the_whole_directory() {
    let mut f = Fixture::new();
    for i in 0..6 {
        f.write(&format!("d{i}/f.bin"), (3 << 20) + i);
    }
    // Loose files in the root, below the tracking threshold.
    f.write("note.txt", 4096);
    f.write("other.txt", 8192);
    f.snapshot();

    let (state, root) = f.finish();
    let l = get(&state, &format!("/api/v1/listing?path={}&from=-7d", enc(&root))).await;

    let rows: i64 = l["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["size"].as_i64().unwrap())
        .sum();
    let own = l["own"]["size"].as_i64().unwrap();
    assert!(own > 0, "loose files should be reported as the directory's own");
    assert_eq!(
        rows + own,
        l["total"].as_i64().unwrap(),
        "children plus own files must equal the directory total"
    );
}

/// A child deleted mid-window must not leave phantom bytes in its sparkline.
///
/// Same failure mode as the stacked area: the deleted path is absent from the
/// end-of-window snapshot, so an attribution walk that only consults that
/// snapshot drops its negative delta.
#[tokio::test]
async fn listing_sparkline_reflects_a_mid_window_deletion() {
    let mut f = Fixture::new();
    f.write("keep/a.bin", 2 << 20);
    f.write("keep/doomed/big.bin", 40 << 20);
    f.snapshot();
    f.snapshot();
    fs::remove_dir_all(f.root.join("keep/doomed")).unwrap();
    f.snapshot();
    f.snapshot();

    let (state, root) = f.finish();
    let l = get(&state, &format!("/api/v1/listing?path={}&from=-7d&points=8", enc(&root))).await;
    let keep = l["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["name"] == "keep")
        .expect("keep should be listed");

    let spark: Vec<i64> = keep["spark"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_i64().unwrap())
        .collect();
    assert!(spark[0] >= 40 << 20, "should start large, got {}", spark[0]);
    assert!(
        *spark.last().unwrap() < 8 << 20,
        "the deletion never showed up in the trend: ends at {}",
        spark.last().unwrap()
    );
    assert_eq!(*spark.last().unwrap(), keep["size"].as_i64().unwrap());
    assert!(keep["delta"].as_i64().unwrap() <= -(40 << 20));
}

/// The Explorer's "go up" row navigates by a path the server resolved, not by
/// one the browser rebuilt from what it is displaying. That matters because a
/// filename that is not valid UTF-8 is *displayed* lossily: trimming the last
/// component off the string on screen produces a path that no longer resolves
/// to the directory it came from.
#[tokio::test]
async fn listing_reports_a_parent_to_navigate_back_to() {
    let mut f = Fixture::new();
    f.write("outer/inner/deep.bin", 6 << 20);
    f.write("outer/sibling.bin", 2 << 20);
    f.snapshot();
    let (state, root) = f.finish();

    // At the scan root there is nowhere further up to go.
    let top = get(&state, &format!("/api/v1/listing?path={}&from=-7d", enc(&root))).await;
    assert!(top["parent"].is_null(), "the root claimed a parent: {}", top["parent"]);

    // One level down, the parent is the root, addressed by its full path.
    let outer = format!("{}/outer", root.display());
    let l = get(&state, &format!("/api/v1/listing?path={}&from=-7d", enc(Path::new(&outer)))).await;
    assert_eq!(l["parent"]["path"]["name"].as_str().unwrap(), root.to_str().unwrap());

    // Two levels down, it is the intermediate directory — and the path it
    // hands back has to be one the API accepts, or the row is a dead end.
    let inner = format!("{}/outer/inner", root.display());
    let l = get(&state, &format!("/api/v1/listing?path={}&from=-7d", enc(Path::new(&inner)))).await;
    assert_eq!(l["parent"]["name"].as_str().unwrap(), "outer");
    assert_eq!(l["parent"]["path"]["name"].as_str().unwrap(), outer);

    let back = l["parent"]["path"]["name"].as_str().unwrap().to_string();
    let up = get(&state, &format!("/api/v1/listing?path={}&from=-7d", enc(Path::new(&back)))).await;
    assert_eq!(up["path"]["name"].as_str().unwrap(), outer);
    let names: Vec<&str> =
        up["rows"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"inner"), "walking up lost the child we came from: {names:?}");

    // The parent's own figures are reported, even though the row renders them
    // only in its tooltip.
    assert!(l["parent"]["size"].as_i64().unwrap() >= 8 << 20);
}
