//! REST API.
//!
//! Everything is JSON under `/api/v1`. Two decisions shape the whole surface:
//!
//! **Paths are bytes, JSON is UTF-8.** Linux filenames are arbitrary byte
//! strings, so every name in a response carries both a lossy display form and
//! the exact bytes in base64, with a flag saying whether they differ. Getting
//! this wrong produces a service that works for two years and then 500s on one
//! user's oddly-named file.
//!
//! **Responses echo the scan they were answered from.** A caller asking for
//! 14:37 gets told the answer is from the 14:35 sample. Silently pretending we
//! have a reading we never took is how a diagnostic tool loses its usefulness.

pub mod state;

use crate::model::{Metric, PathId, RootId, ScanId};
use crate::auth::Viewer;
use crate::store::USABLE_SCAN;
use crate::store::query::{self, Extreme};
use crate::store::snapshot::Snapshot;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;

pub use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/roots", get(roots))
        .route("/api/v1/scans", get(scans))
        .route("/api/v1/resolve", get(resolve))
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/tree", get(tree))
        .route("/api/v1/diff", get(diff))
        .route("/api/v1/series", get(series))
        .route("/api/v1/listing", get(listing))
        .route("/api/v1/gainers", get(gainers))
        .route("/api/v1/auth", get(auth_status))
        .route("/api/v1/cache", get(cache_status))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            crate::auth::middleware,
        ))
        .with_state(state)
}

/// Whether a token is in play, and whether this caller has presented one.
///
/// Unauthenticated on purpose, and it reveals only what the UI needs to
/// decide what to draw: is there a sign-in to offer, and are we signed in.
/// Not the number of protected roots, and certainly not their paths.
async fn auth_status(State(s): State<Arc<AppState>>, viewer: Viewer) -> ApiResult {
    blocking(move || {
        Ok(json!({
            "required": s.has_protected_roots() && !s.auth.is_open(),
            "authenticated": viewer.authed(),
        }))
    })
    .await
}

// ── error plumbing ───────────────────────────────────────────────────────

pub struct ApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let msg = format!("{:#}", self.0);
        tracing::warn!("api error: {msg}");
        (StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))).into_response()
    }
}

type ApiResult = Result<Json<Value>, ApiError>;

/// Run a handler's database work on the blocking pool.
///
/// Every query here is synchronous SQLite. Left on an async worker thread, a
/// few slow requests occupy every worker the runtime has, and the server stops
/// answering anything at all — including the health check that is supposed to
/// tell you it is unwell. Moving them off means a scan, a cold snapshot load
/// and a dashboard refresh can all be in flight at once.
async fn blocking<F>(f: F) -> ApiResult
where
    F: FnOnce() -> anyhow::Result<Value> + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(v)) => Ok(Json(v)),
        Ok(Err(e)) => Err(ApiError(e)),
        Err(e) => Err(ApiError(anyhow::anyhow!("request worker failed: {e}"))),
    }
}

// ── shared helpers ───────────────────────────────────────────────────────

/// Render a filename for JSON without losing information.
fn name_json(name: &std::ffi::OsStr) -> Value {
    let bytes = name.as_bytes();
    match std::str::from_utf8(bytes) {
        Ok(s) => json!({ "name": s, "lossy": false }),
        Err(_) => json!({
            "name": String::from_utf8_lossy(bytes),
            "name_b64": base64::engine::general_purpose::STANDARD.encode(bytes),
            "lossy": true,
        }),
    }
}

fn path_json(p: &std::path::Path) -> Value {
    name_json(p.as_os_str())
}

#[derive(Deserialize)]
pub struct Common {
    pub root: Option<RootId>,
    #[serde(default)]
    pub at: Option<String>,
    #[serde(default)]
    pub metric: Option<String>,
}

fn metric_of(s: &Option<String>) -> Metric {
    match s.as_deref() {
        Some("allocated") | Some("blocks") => Metric::Allocated,
        _ => Metric::Apparent,
    }
}

impl AppState {
    /// Resolve `?root=` to a root id, defaulting to the only/first one.
    /// Resolve the root this request is about, and refuse it if the caller
    /// may not see it.
    ///
    /// An explicit root that is protected is rejected. A request that names
    /// no root gets the first one the caller *can* see, not simply the first
    /// one — otherwise an anonymous visitor whose default happens to be
    /// protected meets an error instead of the data they are allowed.
    fn pick_root(&self, want: Option<RootId>, viewer: Viewer) -> anyhow::Result<RootId> {
        let visible = self.visible_roots(viewer)?;
        match want {
            Some(r) if visible.iter().any(|(id, _)| *id == r) => Ok(r),
            // Deliberately the same message whether the root is protected or
            // absent. Having established that the caller may not see it,
            // confirming it exists would be an odd thing to do next.
            Some(r) => anyhow::bail!("no such root: {r}"),
            None => visible.first().map(|(id, _)| *id).ok_or_else(|| {
                if viewer.authed() {
                    anyhow::anyhow!("no roots tracked yet — run a scan first")
                } else {
                    anyhow::anyhow!("every tracked root is protected; sign in to view them")
                }
            }),
        }
    }

    /// The roots this caller is allowed to know about.
    fn visible_roots(&self, viewer: Viewer) -> anyhow::Result<Vec<(RootId, std::path::PathBuf)>> {
        let store = self.read();
        let mut roots = store.roots()?;
        if !viewer.authed() {
            roots.retain(|(id, _)| !self.is_protected(*id));
        }
        Ok(roots)
    }

    /// Resolve a time expression to a concrete scan, relative to now.
    fn pick_scan(&self, root: RootId, at: &Option<String>) -> anyhow::Result<(ScanId, i64)> {
        self.pick_scan_from(root, at, crate::cli::now())
    }

    /// Resolve a time expression against an arbitrary anchor.
    ///
    /// A window's start is anchored to the moment being *viewed*, not to the
    /// wall clock. With the time slider dragged back three days, "-24h" has
    /// to mean the day before that moment; measuring it from now would show
    /// a window that ends three days before it begins.
    fn pick_scan_from(
        &self,
        root: RootId,
        at: &Option<String>,
        anchor: i64,
    ) -> anyhow::Result<(ScanId, i64)> {
        let now = anchor;
        let spec = at.as_deref().unwrap_or("now");
        let store = self.read();
        match crate::cli::timespec::parse(spec, now)? {
            crate::cli::timespec::Target::Scan(id) => {
                let at: i64 = store.conn.query_row(
                    "SELECT started_at FROM scan WHERE scan_id = ?1",
                    [id],
                    |r| r.get(0),
                )?;
                Ok((id, at))
            }
            crate::cli::timespec::Target::At(t) => query::resolve_scan(&store, root, t)?
                .ok_or_else(|| anyhow::anyhow!("no scan recorded at or before {spec}")),
        }
    }
}

// ── endpoints ────────────────────────────────────────────────────────────

async fn health(State(s): State<Arc<AppState>>) -> ApiResult {
    blocking(move || {
        let store = s.read();
        let roots = store.roots()?;
        let (db, wal) = s.db_bytes();
        Ok(json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            // The full stamp, so the page can say which build is running.
            // A semver is identical across every build between releases, and
            // "is the binary on that server the one I deployed?" is the
            // question this is actually here to answer.
            "build": crate::cli::VERSION,
            "built_at": crate::cli::build_mtime(),
            "db_bytes": db,
            "wal_bytes": wal,
            "repository": option_env!("CARGO_PKG_REPOSITORY").filter(|s| !s.is_empty()),
            "roots": roots.len(),
        }))
    })
    .await
}

/// The roots this caller may see — filtered, not rejected.
///
/// An anonymous caller is not told that a protected root exists. Listing it
/// and refusing to open it would leak the path, and a path like
/// `/mnt/nextcloud/data/steven` is itself information. It also keeps the UI
/// honest: the root picker shows exactly what it can open, so nothing in it
/// is a dead end.
async fn roots(State(s): State<Arc<AppState>>, viewer: Viewer) -> ApiResult {
    blocking(move || {
        let visible = s.visible_roots(viewer)?;
        let store = s.read();
        let mut out = Vec::new();
        for (id, path) in visible {
            let last = store.last_scan(id)?;
            let first = store.first_scan(id)?;
            out.push(json!({
                "root_id": id,
                "path": path_json(&path),
                "scans": store.scan_count(id)?,
                "first_scan": first.map(|(i, at)| json!({"scan_id": i, "at": at})),
                "last_scan": last,
            }));
        }
        Ok(json!({ "roots": out }))
    })
    .await
}

#[derive(Deserialize)]
struct ScansQ {
    root: Option<RootId>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn scans(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<ScansQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let store = s.read();
        let mut st = store.conn.prepare(&format!(
            "SELECT scan_id, started_at, duration_ms, n_events, incl_bytes, incl_blocks,
                    n_dirs, n_files, fs_total, fs_free, fs_avail, status, err
             FROM scan WHERE root_id = ?1 AND {USABLE_SCAN}
             ORDER BY scan_id DESC LIMIT ?2"
        ))?;
        let rows = st.query_map([root, q.limit.unwrap_or(500)], |r| {
            Ok(json!({
                "scan_id": r.get::<_, i64>(0)?,
                "at": r.get::<_, i64>(1)?,
                "duration_ms": r.get::<_, i64>(2)?,
                "events": r.get::<_, i64>(3)?,
                "bytes": r.get::<_, i64>(4)?,
                "blocks": r.get::<_, i64>(5)?,
                "dirs": r.get::<_, i64>(6)?,
                "files": r.get::<_, i64>(7)?,
                "fs_total": r.get::<_, Option<i64>>(8)?,
                "fs_free": r.get::<_, Option<i64>>(9)?,
                "fs_avail": r.get::<_, Option<i64>>(10)?,
            }))
        })?;
        let mut v: Vec<Value> = rows.collect::<rusqlite::Result<_>>()?;
        v.reverse();
        Ok(json!({ "root_id": root, "scans": v }))
    })
    .await
}

async fn resolve(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<Common>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let (scan_id, at) = s.pick_scan(root, &q.at)?;
        Ok(json!({ "scan_id": scan_id, "at": at }))
    })
    .await
}

/// Everything the landing page needs, in one round trip.
async fn overview(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<Common>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let metric = metric_of(&q.metric);
        let (scan_id, at) = s.pick_scan(root, &q.at)?;

        let (path, first, total_scans, fs, partial, fstype, history) = {
            let store = s.read();
            let path = store.root_path(root)?;
            let first = store.first_scan(root)?;
            let total = store.scan_count(root)?;
            let fs: (Option<i64>, Option<i64>, Option<i64>) = store.conn.query_row(
                "SELECT fs_total, fs_free, fs_avail FROM scan WHERE scan_id = ?1",
                [scan_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            // A scan that could not read everything reports a total that is
            // too low. That has to reach the screen: the whole point of this
            // page is that the number on it means something.
            let partial: (String, Option<String>) = store.conn.query_row(
                "SELECT status, err FROM scan WHERE scan_id = ?1",
                [scan_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            // What to do about an unreadable path depends on where it lives,
            // so resolve the root's filesystem now rather than letting the UI
            // offer advice that cannot work on a network share.
            let fstype = crate::scan::mounts::MountTable::load()
                .ok()
                .and_then(|mt| mt.find_mount_for(&path).map(|e| e.fstype.clone()));
            let mut st = store.conn.prepare(&format!(
                "SELECT started_at, incl_bytes, incl_blocks, fs_free FROM scan
                 WHERE root_id = ?1 AND {USABLE_SCAN} ORDER BY scan_id"
            ))?;
            let rows = st.query_map([root], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?;
            let history: Vec<(i64, i64, i64, Option<i64>)> = rows.collect::<rusqlite::Result<_>>()?;
            (path, first, total, fs, partial, fstype, history)
        };

        let series: Vec<Value> = history
            .iter()
            .map(|(t, b, k, free)| {
                json!([t, if metric == Metric::Allocated { k } else { b }, free])
            })
            .collect();

        // Days until full, from the trend in free space.
        //
        // Theil-Sen rather than least squares: the median of pairwise slopes is
        // unmoved by a one-off 20 GB download, where an ordinary regression would
        // swing wildly and predict the disk filling next Tuesday.
        let free_points: Vec<(f64, f64)> = history
            .iter()
            .filter_map(|(t, _, _, f)| f.map(|f| (*t as f64, f as f64)))
            .collect();
        // Refuse to extrapolate from a window too short to mean anything.
        //
        // Without this guard, five scans taken 40 seconds apart while a 3 GB file
        // was being written produce "disk full in 2.9 days" — stated with total
        // confidence and wrong by orders of magnitude. A forecast that cries wolf
        // once is never trusted again, so say "not yet" until there is a real
        // window to extrapolate from.
        const MIN_FORECAST_SPAN_S: f64 = 6.0 * 3600.0;
        const MIN_FORECAST_POINTS: usize = 6;

        let span = match (free_points.first(), free_points.last()) {
            (Some(a), Some(b)) => b.0 - a.0,
            _ => 0.0,
        };
        let forecast = if free_points.len() < MIN_FORECAST_POINTS || span < MIN_FORECAST_SPAN_S {
            json!({
                "status": "insufficient_history",
                "needs": {
                    "samples": MIN_FORECAST_POINTS,
                    "span_hours": MIN_FORECAST_SPAN_S / 3600.0,
                    "have_samples": free_points.len(),
                    "have_span_hours": span / 3600.0,
                }
            })
        } else {
            match theil_sen(&free_points) {
                None => json!({ "status": "insufficient_history" }),
                Some(slope_per_s) => {
                    let last_free = free_points.last().map(|p| p.1).unwrap_or(0.0);
                    let per_day = slope_per_s * 86_400.0;
                    if slope_per_s >= -1e-9 {
                        json!({ "status": "ok", "trend_bytes_per_day": per_day, "days_to_full": null })
                    } else {
                        json!({
                            "status": "ok",
                            "trend_bytes_per_day": per_day,
                            "days_to_full": (last_free / -slope_per_s) / 86_400.0,
                        })
                    }
                }
            }
        };

        Ok(json!({
            "root_id": root,
            "path": path_json(&path),
            "scan_id": scan_id,
            "at": at,
            "scans": total_scans,
            "first_scan": first.map(|(i, a)| json!({"scan_id": i, "at": a})),
            "fs": { "total": fs.0, "free": fs.1, "avail": fs.2 },
            "history": series,
            "forecast": forecast,
            "scan_status": partial.0,
            "scan_error": partial.1,
            "fstype": fstype,
            "server_authorized": fstype.as_deref().is_some_and(crate::scan::mounts::is_server_authorized),
        }))
    })
    .await
}

/// Median of pairwise slopes. Returns `None` with fewer than two points.
fn theil_sen(pts: &[(f64, f64)]) -> Option<f64> {
    if pts.len() < 2 {
        return None;
    }
    // Cap the pair count so a year of five-minute samples doesn't turn a
    // dashboard load into 5 billion slope computations.
    let step = ((pts.len() * pts.len()) / 20_000).max(1);
    let mut slopes: Vec<f64> = Vec::new();
    let mut c = 0usize;
    for i in 0..pts.len() {
        for j in (i + 1)..pts.len() {
            c += 1;
            if c % step != 0 {
                continue;
            }
            let dt = pts[j].0 - pts[i].0;
            if dt.abs() > f64::EPSILON {
                slopes.push((pts[j].1 - pts[i].1) / dt);
            }
        }
    }
    if slopes.is_empty() {
        return None;
    }
    slopes.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(slopes[slopes.len() / 2])
}

#[derive(Deserialize)]
struct TreeQ {
    root: Option<RootId>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    metric: Option<String>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The treemap feed: a bounded slice of the tree as it stood at one instant.
async fn tree(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<TreeQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let metric = metric_of(&q.metric);
        let (scan_id, at) = s.pick_scan(root, &q.at)?;
        let snap = s.snapshot(root, scan_id)?;

        let node = locate(&snap, q.path.as_deref())?;
        let depth = q.depth.unwrap_or(2).min(6);
        let limit = q.limit.unwrap_or(200).min(2000);

        let value = |i: u32| -> i64 {
            let i = i as usize;
            if metric == Metric::Allocated { snap.incl_blocks[i] } else { snap.incl_bytes[i] }
        };

        Ok(json!({
            "root_id": root,
            "scan_id": scan_id,
            "at": at,
            "path": path_json(&snap.path_of(node)),
            "total": value(node),
            "node": build_node(&snap, node, depth, limit, metric, &value),
        }))
    })
    .await
}

/// Recursively emit a node and its largest children.
///
/// Children beyond `limit` are folded into a single synthesized "other" entry
/// rather than dropped. A treemap whose rectangles do not add up to their
/// parent is worse than useless — it quietly misattributes space.
fn build_node(
    snap: &Snapshot,
    i: u32,
    depth: u32,
    limit: usize,
    metric: Metric,
    value: &dyn Fn(u32) -> i64,
) -> Value {
    let mut base = name_json(&snap.name[i as usize]);
    let obj = base.as_object_mut().unwrap();
    obj.insert("id".into(), json!(snap.ids[i as usize]));
    obj.insert("value".into(), json!(value(i)));
    obj.insert("own".into(), json!(snap.own_bytes[i as usize]));
    obj.insert("kind".into(), json!(kind_str(snap.kind[i as usize])));
    obj.insert("files".into(), json!(snap.incl_files[i as usize]));
    obj.insert("dirs".into(), json!(snap.incl_dirs[i as usize]));

    if depth == 0 {
        let n = snap.children[i as usize].len();
        if n > 0 {
            obj.insert("truncated".into(), json!(n));
        }
        return base;
    }

    let mut kids: Vec<u32> = snap.children[i as usize].clone();
    kids.sort_by_key(|&c| std::cmp::Reverse(value(c)));

    let shown = kids.len().min(limit);
    let mut out: Vec<Value> = kids[..shown]
        .iter()
        .filter(|&&c| value(c) > 0)
        .map(|&c| build_node(snap, c, depth - 1, limit, metric, value))
        .collect();

    let rest: i64 = kids[shown..].iter().map(|&c| value(c)).sum();
    if rest > 0 {
        out.push(json!({
            "name": format!("<{} more>", kids.len() - shown),
            "lossy": false,
            "value": rest,
            "kind": "other",
            "synthetic": true,
        }));
    }

    // Space held directly by this directory, so children sum to the parent.
    let own = match metric {
        Metric::Allocated => snap.own_blocks[i as usize],
        Metric::Apparent => snap.own_bytes[i as usize],
    };
    if own > 0 && !out.is_empty() {
        out.push(json!({
            "name": "<files here>",
            "lossy": false,
            "value": own,
            "kind": "own",
            "synthetic": true,
        }));
    }

    if !out.is_empty() {
        obj.insert("children".into(), json!(out));
    }
    base
}

fn kind_str(k: crate::model::Kind) -> &'static str {
    match k {
        crate::model::Kind::Dir => "dir",
        crate::model::Kind::File => "file",
        crate::model::Kind::Symlink => "symlink",
        crate::model::Kind::Other => "other",
    }
}

fn locate(snap: &Snapshot, path: Option<&str>) -> anyhow::Result<u32> {
    let root = snap.root().ok_or_else(|| anyhow::anyhow!("snapshot has no root"))?;
    let Some(p) = path.filter(|p| !p.is_empty()) else { return Ok(root) };
    let target = std::path::Path::new(p);
    let root_path = snap.path_of(root);
    let rel = target.strip_prefix(&root_path).unwrap_or(target);
    let comps: Vec<std::ffi::OsString> = rel.iter().map(|c| c.to_os_string()).collect();
    if comps.is_empty() {
        return Ok(root);
    }
    snap.resolve(&comps).ok_or_else(|| {
        anyhow::anyhow!("{p} is not tracked at this time (below the size threshold, excluded, or not yet created)")
    })
}

#[derive(Deserialize)]
struct DiffQ {
    root: Option<RootId>,
    #[serde(default)]
    path: Option<String>,
    from: String,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    metric: Option<String>,
    #[serde(default)]
    depth: Option<u32>,
    #[serde(default)]
    limit: Option<usize>,
}

/// The diff treemap feed: rectangle area is size at `to`, colour is the change.
async fn diff(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<DiffQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let metric = metric_of(&q.metric);
        let (s1, at1) = s.pick_scan(root, &Some(q.from.clone()))?;
        let (s2, at2) = s.pick_scan(root, &q.to)?;

        let a = s.snapshot(root, s1)?;
        let b = s.snapshot(root, s2)?;
        let node = locate(&b, q.path.as_deref())?;
        let depth = q.depth.unwrap_or(3).min(6);
        let limit = q.limit.unwrap_or(200).min(2000);

        let val = |snap: &Snapshot, i: u32| -> i64 {
            if metric == Metric::Allocated {
                snap.incl_blocks[i as usize]
            } else {
                snap.incl_bytes[i as usize]
            }
        };

        // A diff tile is sized by max(before, after), not by its size at the end.
        //
        // Sizing by the later value makes anything that was deleted have zero area
        // and vanish from the picture entirely — but "a 700 MB download
        // disappeared" is exactly half of what this view exists to answer, and
        // it is the half a point-in-time `du` can never tell you. Giving a tile
        // the larger of its two sizes keeps deletions on screen at the scale they
        // actually mattered, coloured blue.
        struct Ctx<'a> {
            a: &'a Snapshot,
            b: &'a Snapshot,
            metric: Metric,
        }
        impl Ctx<'_> {
            fn size(&self, snap: &Snapshot, i: u32) -> i64 {
                match self.metric {
                    Metric::Allocated => snap.incl_blocks[i as usize],
                    Metric::Apparent => snap.incl_bytes[i as usize],
                }
            }
            fn before(&self, id: PathId) -> i64 {
                self.a.idx(id).map(|i| self.size(self.a, i)).unwrap_or(0)
            }
            fn after(&self, id: PathId) -> i64 {
                self.b.idx(id).map(|i| self.size(self.b, i)).unwrap_or(0)
            }
            fn name_of(&self, id: PathId) -> std::ffi::OsString {
                self.b
                    .idx(id)
                    .map(|i| self.b.name[i as usize].clone())
                    .or_else(|| self.a.idx(id).map(|i| self.a.name[i as usize].clone()))
                    .unwrap_or_default()
            }
            fn kind_of(&self, id: PathId) -> crate::model::Kind {
                self.b
                    .idx(id)
                    .map(|i| self.b.kind[i as usize])
                    .or_else(|| self.a.idx(id).map(|i| self.a.kind[i as usize]))
                    .unwrap_or(crate::model::Kind::Other)
            }
            /// Children present in either snapshot, so deletions are not lost.
            fn children(&self, id: PathId) -> Vec<PathId> {
                let mut out: Vec<PathId> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for (snap, idx) in [(self.b, self.b.idx(id)), (self.a, self.a.idx(id))] {
                    if let Some(i) = idx {
                        for &c in &snap.children[i as usize] {
                            let cid = snap.ids[c as usize];
                            if seen.insert(cid) {
                                out.push(cid);
                            }
                        }
                    }
                }
                out
            }
        }

        let ctx = Ctx { a: &a, b: &b, metric };

        fn walk(ctx: &Ctx, id: PathId, depth: u32, limit: usize) -> Value {
            let then = ctx.before(id);
            let now = ctx.after(id);
            let area = then.max(now);

            let mut base = name_json(&ctx.name_of(id));
            let obj = base.as_object_mut().unwrap();
            obj.insert("id".into(), json!(id));
            obj.insert("value".into(), json!(area));
            obj.insert("after".into(), json!(now));
            obj.insert("before".into(), json!(then));
            obj.insert("delta".into(), json!(now - then));
            obj.insert("gone".into(), json!(now == 0 && then > 0));
            obj.insert("kind".into(), json!(kind_str(ctx.kind_of(id))));

            if depth > 0 {
                let mut kids = ctx.children(id);
                kids.sort_by_key(|&c| std::cmp::Reverse(ctx.before(c).max(ctx.after(c))));
                let shown = kids.len().min(limit);
                let out: Vec<Value> = kids[..shown]
                    .iter()
                    .filter(|&&c| ctx.before(c).max(ctx.after(c)) > 0)
                    .map(|&c| walk(ctx, c, depth - 1, limit))
                    .collect();
                if !out.is_empty() {
                    obj.insert("children".into(), json!(out));
                }
            }
            base
        }

        let node_id = b.ids[node as usize];
        let tree = walk(&ctx, node_id, depth, limit);
        let _ = val;

        Ok(json!({
            "root_id": root,
            "from": { "scan_id": s1, "at": at1 },
            "to":   { "scan_id": s2, "at": at2 },
            "path": path_json(&b.path_of(node)),
            "node": tree,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct SeriesQ {
    root: Option<RootId>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    metric: Option<String>,
    #[serde(default)]
    children: Option<usize>,
}

/// Stacked-area feed: a directory decomposed into its largest children over
/// time, plus an explicit band for everything else.
///
/// Computed by seeding each band with its size at the start of the window and
/// then attributing every event in the window to whichever band contains it —
/// one ancestor walk per event. Cost tracks churn, not tree size, so a window
/// over a 127k-entity tree with 50 changes costs ~50 x depth operations rather
/// than replaying 127k entities per sample.
async fn series(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<SeriesQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let metric = metric_of(&q.metric);
        let (s2, at2) = s.pick_scan(root, &q.to)?;
        let (s1, at1) = s
            .pick_scan_from(root, &Some(q.from.clone().unwrap_or_else(|| "-7d".into())), at2)
            .or_else(|_| {
                let st = s.read();
                st.first_scan(root)?
                    .ok_or_else(|| anyhow::anyhow!("no scans recorded"))
                
            })?;

        let snap = s.snapshot(root, s2)?;
        let node = locate(&snap, q.path.as_deref())?;
        let top_n = q.children.unwrap_or(8).clamp(1, 24);

        let val = |i: u32| -> i64 {
            if metric == Metric::Allocated {
                snap.incl_blocks[i as usize]
            } else {
                snap.incl_bytes[i as usize]
            }
        };

        let mut kids: Vec<u32> = snap.children[node as usize].clone();
        kids.sort_by_key(|&c| std::cmp::Reverse(val(c)));
        let bands: Vec<u32> = kids.iter().copied().take(top_n).collect();

        // band index per path_id, for the ancestor walk below
        // One downward pass; see `band_map`. The map is indexed by snapshot
        // position, and the fallback map by path id for anything deleted
        // mid-window and therefore absent from the snapshot.
        let bandmap = band_map(&snap, node, &bands);
        let mut band_of: std::collections::HashMap<PathId, usize> = Default::default();
        for (bi, &k) in bands.iter().enumerate() {
            band_of.insert(snap.ids[k as usize], bi);
        }
        const OTHER: usize = usize::MAX - 1;

        let store = s.read();

        // Parent links for everything that existed at any point in the window,
        // not just what survives to the end.
        //
        // The end-of-window snapshot cannot answer this. A directory deleted
        // mid-window emits its (large, negative) event and then vanishes from the
        // tree, so looking it up in the final snapshot finds nothing and the
        // deletion is silently dropped — the band keeps the bytes forever and the
        // stack drifts above the real total. Caught on demo data as a 16.8 MB
        // overstatement of /var after a package cache was cleared.
        let mut ancestry = Ancestry::new(&store.conn);
        let node_id = snap.ids[node as usize];

        // Starting values are filled in after the window's events are read:
        // each band's size now, minus what the window did to it.
        //
        // This used to call `incl_at` once per band, which reconstructs a
        // subtree total by walking every descendant. For a child of a 1.3M
        // entity root that is a million rows, nine times per request, and it
        // measured at 2.8 seconds with every cache already warm.
        let mut level: Vec<i64> = vec![0; bands.len() + 1];

        // Every scan in the window becomes an x position.
        let mut st = store.conn.prepare(&format!(
            "SELECT scan_id, started_at FROM scan
             WHERE root_id = ?1 AND scan_id >= ?2 AND scan_id <= ?3 AND {USABLE_SCAN}
             ORDER BY scan_id"
        ))?;
        let scan_rows = st.query_map(params_3(root, s1, s2), |r| {
            Ok((r.get::<_, ScanId>(0)?, r.get::<_, i64>(1)?))
        })?;
        let scan_list: Vec<(ScanId, i64)> = scan_rows.collect::<rusqlite::Result<_>>()?;

        // Deltas in the window, grouped by scan.
        let mut ev = store.conn.prepare(
            "SELECT e.scan_id, e.path_id, e.d_bytes, e.d_blocks
             FROM size_event e JOIN path p ON p.path_id = e.path_id
             WHERE p.root_id = ?1 AND e.scan_id > ?2 AND e.scan_id <= ?3
             ORDER BY e.scan_id",
        )?;
        let evs = ev.query_map(params_3(root, s1, s2), |r| {
            Ok((r.get::<_, ScanId>(0)?, r.get::<_, PathId>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        })?;

        let mut by_scan: std::collections::HashMap<ScanId, Vec<(usize, i64)>> = Default::default();
        for e in evs {
            let (sid, pid, db, dk) = e?;
            let d = if metric == Metric::Allocated { dk } else { db };
            if d == 0 {
                continue;
            }
            // Which band holds this path.
            let slot = match snap.idx(pid) {
                Some(i) => match bandmap[i as usize] {
                    NO_BAND => continue,
                    OTHER_BAND => OTHER,
                    b => b as usize,
                },
                None => match ancestry.band(&band_of, node_id, pid)? {
                    Climb::Band(b) => b,
                    Climb::Own => OTHER,
                    Climb::Outside => continue,
                },
            };
            by_scan.entry(sid).or_default().push((slot, d));
        }

        let n_bands = level.len();

        // Wind back from the end of the window to its start: every band's
        // size now, less what the window's events did to it. The last band is
        // "everything else" — whatever the directory holds beyond its top
        // children, which is its total less those children.
        {
            let mut totals = vec![0i64; n_bands];
            for deltas in by_scan.values() {
                for &(slot, d) in deltas {
                    totals[if slot == OTHER { n_bands - 1 } else { slot.min(n_bands - 1) }] += d;
                }
            }
            let kids_now: i64 = bands.iter().map(|&k| val(k)).sum();
            for (i, &k) in bands.iter().enumerate() {
                level[i] = val(k) - totals[i];
            }
            level[n_bands - 1] = (val(node) - kids_now) - totals[n_bands - 1];
        }
        let mut points: Vec<Vec<i64>> = vec![Vec::with_capacity(scan_list.len()); n_bands];
        let mut times: Vec<i64> = Vec::with_capacity(scan_list.len());
        for (sid, t) in &scan_list {
            if let Some(deltas) = by_scan.get(sid) {
                for &(slot, d) in deltas {
                    let idx = if slot == OTHER { n_bands - 1 } else { slot.min(n_bands - 1) };
                    level[idx] += d;
                }
            }
            times.push(*t);
            for b in 0..n_bands {
                points[b].push(level[b]);
            }
        }

        let mut out: Vec<Value> = bands
            .iter()
            .enumerate()
            .map(|(bi, &k)| {
                let mut v = name_json(&snap.name[k as usize]);
                let o = v.as_object_mut().unwrap();
                o.insert("id".into(), json!(snap.ids[k as usize]));
                o.insert("points".into(), json!(points[bi]));
                v
            })
            .collect();
        out.push(json!({
            "name": if kids.len() > bands.len() {
                format!("<other, incl. {} more>", kids.len() - bands.len())
            } else {
                "<files here>".to_string()
            },
            "lossy": false,
            "synthetic": true,
            "points": points[n_bands - 1],
        }));

        Ok(json!({
            "root_id": root,
            "path": path_json(&snap.path_of(node)),
            "from": { "scan_id": s1, "at": at1 },
            "to": { "scan_id": s2, "at": at2 },
            "times": times,
            "bands": out,
        }))
    })
    .await
}

fn params_3(a: i64, b: i64, c: i64) -> [i64; 3] {
    [a, b, c]
}

#[derive(Deserialize)]
struct GainersQ {
    root: Option<RootId>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    losers: Option<bool>,
    #[serde(default)]
    collapse: Option<bool>,
}

#[derive(Serialize)]
struct GainerOut {
    path_id: PathId,
    path: Value,
    delta: i64,
    delta_blocks: i64,
}

async fn gainers(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<GainersQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let (s2, at2) = s.pick_scan(root, &q.to)?;
        let inclusive = q.mode.as_deref() == Some("inclusive");
        let which = if q.losers.unwrap_or(false) { Extreme::Losers } else { Extreme::Gainers };
        let limit = q.limit.unwrap_or(25).clamp(1, 500);
        let collapse = q.collapse.unwrap_or(true) && inclusive;

        // Clamp to the first scan rather than to zero: a window that reaches past
        // the start of tracking would otherwise report the baseline's births as
        // growth and claim the entire disk appeared inside the window.
        let (s1, at1, clamped) = {
            let store = s.read();
            let now = crate::cli::now();
            let want = match crate::cli::timespec::parse(
                q.from.as_deref().unwrap_or("-24h"),
                now,
            )? {
                crate::cli::timespec::Target::Scan(id) => {
                    let at = store.conn.query_row(
                        "SELECT started_at FROM scan WHERE scan_id = ?1",
                        [id],
                        |r| r.get(0),
                    )?;
                    Some((id, at))
                }
                crate::cli::timespec::Target::At(t) => query::resolve_scan(&store, root, t)?,
            };
            match want {
                Some((id, at)) => (id, at, false),
                None => {
                    let (id, at) = store
                        .first_scan(root)?
                        .ok_or_else(|| anyhow::anyhow!("no scans recorded"))?;
                    (id, at, true)
                }
            }
        };

        let list = {
            let store = s.read();
            let fetch = if collapse { limit * 8 } else { limit };
            let mut g = if inclusive {
                query::gainers_inclusive(&store, root, s1, s2, fetch, which)?
            } else {
                query::gainers_exclusive(&store, root, s1, s2, fetch, which)?
            };
            if collapse {
                g = query::collapse_ancestors(&g, 0.9);
            }
            g.truncate(limit as usize);
            g.into_iter()
                .map(|e| {
                    Ok(GainerOut {
                        path_id: e.path_id,
                        path: path_json(&query::full_path(&store, e.path_id)?),
                        delta: e.delta_bytes,
                        delta_blocks: e.delta_blocks,
                    })
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };

        Ok(json!({
            "root_id": root,
            "from": { "scan_id": s1, "at": at1 },
            "to": { "scan_id": s2, "at": at2 },
            "window_clamped_to_first_scan": clamped,
            "mode": if inclusive { "inclusive" } else { "exclusive" },
            "collapsed": collapse,
            "results": list,
        }))
    })
    .await
}


/// Not in any band.
const NO_BAND: u32 = u32::MAX;
/// In the viewed directory's subtree, but not under any listed child.
const OTHER_BAND: u32 = u32::MAX - 1;

/// Which band every node in `node`'s subtree belongs to, by snapshot index.
///
/// Built once per request by walking down from the directory being viewed,
/// so resolving an event afterwards is a single array index. The obvious
/// alternative — climb from each event up to whichever child contains it —
/// costs depth lookups *per event*, and a window that includes a baseline
/// scan holds one event per entity. On a 1.3M-entity volume that measured at
/// 4.9 seconds for the listing; walking down instead is one pass over the
/// same tree and it is already in memory.
///
/// `kids[b]` is the snapshot index of band `b`. Nodes under the directory but
/// not under any listed child — including the directory itself, whose events
/// are its own files — get [`OTHER_BAND`]; everything outside stays
/// [`NO_BAND`].
fn band_map(snap: &Snapshot, node: u32, kids: &[u32]) -> Vec<u32> {
    let mut map = vec![NO_BAND; snap.len()];
    let mut of_kid: std::collections::HashMap<u32, u32> = Default::default();
    for (b, &k) in kids.iter().enumerate() {
        of_kid.insert(k, b as u32);
    }

    map[node as usize] = OTHER_BAND;
    // Iterative, not recursive: these trees are a million nodes deep in the
    // pathological case and a recursive walk would blow the stack.
    let mut stack: Vec<(u32, u32)> = snap.children[node as usize]
        .iter()
        .map(|&c| (c, of_kid.get(&c).copied().unwrap_or(OTHER_BAND)))
        .collect();
    while let Some((idx, band)) = stack.pop() {
        map[idx as usize] = band;
        for &c in &snap.children[idx as usize] {
            stack.push((c, band));
        }
    }
    map
}

/// Parent links, fetched one at a time and remembered.
///
/// Both the stacked area and the listing need to know which child of the
/// directory being viewed each event belongs under, which means walking up
/// from the event's path. The obvious way to do that is to load
/// `path_id -> parent_id` for the whole root into a map, and that is what
/// this used to do — 1.3M rows built into a HashMap on every request, which
/// measured at 320ms for the listing and did not improve with any cache,
/// because the cost was the load itself.
///
/// A window holds a few hundred events and the mean directory depth is around
/// nine, so the climb touches a few thousand rows at the very most, and
/// usually far fewer once the memo starts hitting. Each is a primary-key
/// lookup.
///
/// Deliberately *not* filtered by liveness: an event may belong to a path
/// deleted mid-window, and its ancestors may be gone too, but the rows are
/// still there and the climb still has to work. Filtering on `died_scan`
/// would silently drop the deletions — the very events that explain a drop.
struct Ancestry<'a> {
    conn: &'a rusqlite::Connection,
    parent: std::collections::HashMap<PathId, Option<PathId>>,
}

impl<'a> Ancestry<'a> {
    fn new(conn: &'a rusqlite::Connection) -> Self {
        Self { conn, parent: Default::default() }
    }

    fn parent_of(&mut self, id: PathId) -> anyhow::Result<Option<PathId>> {
        if let Some(&p) = self.parent.get(&id) {
            return Ok(p);
        }
        let p: Option<PathId> = self
            .conn
            .query_row("SELECT parent_id FROM path WHERE path_id = ?1", [id], |r| r.get(0))
            .optional()?
            .flatten();
        self.parent.insert(id, p);
        Ok(p)
    }

    /// Climb from `from` to whichever entry of `bands` contains it.
    ///
    /// `Ok(None)` means the climb left the subtree without meeting one —
    /// either it reached `node_id` itself (the directory's own files) or ran
    /// off the top of the tree.
    fn band(
        &mut self,
        bands: &std::collections::HashMap<PathId, usize>,
        node_id: PathId,
        from: PathId,
    ) -> anyhow::Result<Climb> {
        let mut cur = from;
        // Bounded so a cycle in the dictionary cannot hang a request. Real
        // depth is single digits; 512 is far past anything a filesystem
        // produces and still terminates instantly if the data is corrupt.
        for _ in 0..512 {
            if let Some(&b) = bands.get(&cur) {
                return Ok(Climb::Band(b));
            }
            if cur == node_id {
                return Ok(Climb::Own);
            }
            match self.parent_of(cur)? {
                Some(p) => cur = p,
                None => return Ok(Climb::Outside),
            }
        }
        Ok(Climb::Outside)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Climb {
    /// Landed on one of the listed children.
    Band(usize),
    /// Reached the viewed directory itself: its own files, no child.
    Own,
    /// Left the subtree entirely.
    Outside,
}

// ── directory listing with trend sparklines ──────────────────────────────

#[derive(Deserialize)]
struct ListingQ {
    root: Option<RootId>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    at: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    metric: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    /// How many points each sparkline carries.
    #[serde(default)]
    points: Option<usize>,
}

/// One row per child: what it is now, how it changed, and its recent shape.
///
/// The listing answers a different question from the treemap. A treemap is
/// good at "what is big"; it is poor at "is this one creeping up", because a
/// rectangle that grew 8% looks like a rectangle. A sorted list with a trend
/// beside each row lets you see which of thirty sibling directories is the one
/// moving, before deciding which to open.
///
/// Sparklines are computed the same way the stacked area is: seed each child
/// with its size at the start of the window, then attribute every event in the
/// window to whichever child contains it. Cost tracks churn, not tree size.
async fn listing(
    State(s): State<Arc<AppState>>,
    viewer: Viewer,
    Query(q): Query<ListingQ>,
) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root, viewer)?;
        let metric = metric_of(&q.metric);
        let (s2, at2) = s.pick_scan(root, &q.at)?;

        // Clamp to the first scan rather than reaching past the start of
        // history, which would report the baseline's births as growth.
        let (s1, at1, clamped) = {
            let store = s.read();
            // Anchored to the moment being viewed, not the wall clock: with
            // the slider dragged back, "-24h" is the day before *that*.
            let want = match crate::cli::timespec::parse(
                q.from.as_deref().unwrap_or("-7d"),
                at2,
            )? {
                crate::cli::timespec::Target::Scan(id) => {
                    let at = store.conn.query_row(
                        "SELECT started_at FROM scan WHERE scan_id = ?1",
                        [id],
                        |r| r.get(0),
                    )?;
                    Some((id, at))
                }
                crate::cli::timespec::Target::At(t) => query::resolve_scan(&store, root, t)?,
            };
            match want {
                Some((id, at)) if id <= s2 => (id, at, false),
                _ => {
                    let (id, at) = store
                        .first_scan(root)?
                        .ok_or_else(|| anyhow::anyhow!("no scans recorded"))?;
                    (id, at, true)
                }
            }
        };

        // One snapshot, not two. Sizes at the start of the window are derived
        // from the end of it by subtracting the window's own events — the
        // invariant the store is built on — rather than by materialising the
        // whole tree a second time. On a 1.3M-entity volume that second build
        // measured at two seconds, and it happened every time the directory
        // listing was drawn.
        let snap = s.snapshot(root, s2)?;
        let node = locate(&snap, q.path.as_deref())?;
        let limit = q.limit.unwrap_or(500).min(5000);
        let points = q.points.unwrap_or(32).clamp(2, 240);

        let size_of = |sn: &Snapshot, i: u32| -> i64 {
            match metric {
                Metric::Allocated => sn.incl_blocks[i as usize],
                Metric::Apparent => sn.incl_bytes[i as usize],
            }
        };

        // Every child, measured before any is dropped.
        let all: Vec<u32> = snap.children[node as usize].clone();
        let w = window_deltas(&s, root, &snap, node, &all, s1, s2, metric)?;

        // Rank by what moved, then by what is big.
        //
        // Ranking by size alone is what this replaces, and it had a bad
        // failure: in a directory of thousands, a small folder that doubled
        // sat below a thousand large ones that did nothing, fell outside the
        // limit, and vanished into the "not shown" count — so the one entry
        // worth seeing was the one you could not see. Size still decides the
        // order among things that did not move, and still decides the display
        // order in the browser; this only decides who makes the cut.
        let mut order: Vec<usize> = (0..all.len()).collect();
        order.sort_by(|&a, &b| {
            (w.totals[b] != 0)
                .cmp(&(w.totals[a] != 0))
                .then_with(|| w.totals[b].abs().cmp(&w.totals[a].abs()))
                .then_with(|| size_of(&snap, all[b]).cmp(&size_of(&snap, all[a])))
        });
        let (pick, rest) = order.split_at(order.len().min(limit));
        // How many of the omitted actually changed. Normally zero — the
        // movers are taken first — and when it is zero the UI can say the
        // hidden entries are unchanged rather than merely smaller.
        let omitted_changed = rest.iter().filter(|&&i| w.totals[i] != 0).count();

        let kids: Vec<u32> = pick.iter().map(|&i| all[i]).collect();
        // Seeded with the size *now*; `series_for` winds each back to the
        // start of the window using the deltas already computed.
        let mut level: Vec<i64> = kids.iter().map(|&c| size_of(&snap, c)).collect();
        let spark = series_for(pick, &w, &mut level, points);
        let totals: Vec<i64> = pick.iter().map(|&i| w.totals[i]).collect();
        let own_total = w.own_total;

        // The row that walks back up. Resolved here rather than by trimming
        // the displayed path in the browser: a path component that is not
        // valid UTF-8 is displayed lossily, so a string built from what is on
        // screen would not resolve back to the directory it came from.
        let parent = match snap.parent[node as usize] {
            crate::model::tree::NO_PARENT => Value::Null,
            p => {
                let pi = p as usize;
                // Size only. The parent's change over the window would need
                // the events under *its* whole subtree, which is a different
                // and much larger question than this listing asks — and it
                // appears nowhere but the up row's tooltip.
                let mut v = name_json(&snap.name[pi]);
                let o = v.as_object_mut().unwrap();
                o.insert("id".into(), json!(snap.ids[pi]));
                o.insert("path".into(), path_json(&snap.path_of(p)));
                o.insert("size".into(), json!(size_of(&snap, p)));
                v
            }
        };

        let own_now = match metric {
            Metric::Allocated => snap.own_blocks[node as usize],
            Metric::Apparent => snap.own_bytes[node as usize],
        };
        let own_then = own_now - own_total;

        let rows: Vec<Value> = kids
            .iter()
            .enumerate()
            .map(|(bi, &c)| {
                let i = c as usize;
                let now = size_of(&snap, c);
                let then = now - totals[bi];
                let mut v = name_json(&snap.name[i]);
                let o = v.as_object_mut().unwrap();
                o.insert("id".into(), json!(snap.ids[i]));
                o.insert("kind".into(), json!(kind_str(snap.kind[i])));
                o.insert("size".into(), json!(now));
                o.insert("before".into(), json!(then));
                o.insert("delta".into(), json!(now - then));
                o.insert("files".into(), json!(snap.incl_files[i]));
                o.insert("dirs".into(), json!(snap.incl_dirs[i]));
                o.insert("spark".into(), json!(spark[bi]));
                v
            })
            .collect();

        Ok(json!({
            "root_id": root,
            "path": path_json(&snap.path_of(node)),
            "total": size_of(&snap, node),
            "from": { "scan_id": s1, "at": at1 },
            "to": { "scan_id": s2, "at": at2 },
            "window_clamped_to_first_scan": clamped,
            "parent": parent,
            "truncated": rest.len(),
            // Zero means every hidden entry is unchanged, which is a much
            // more reassuring thing to be told than "some were hidden".
            "truncated_changed": omitted_changed,
            "own": {
                "size": own_now,
                "delta": own_now - own_then,
                "files": snap.own_files[node as usize],
            },
            "rows": rows,
        }))
    })
    .await
}

/// Per-child value series across the window, downsampled to `points`.
///
/// `level` arrives holding each child's size at the **start** of the window;
/// the caller derives that by subtracting the totals this function returns
/// from the sizes it already has at the end of the window, so no second
/// snapshot of the tree is needed. The totals are returned alongside the
/// series for that purpose.
///
/// Two passes over the window's events: one to resolve each to a child and
/// total it, one to replay them in order. Events number in the hundreds even
/// on a large volume — the storage is change-only — so the second pass costs
/// nothing next to materialising a whole tree.
/// What the window did to every child of a directory.
///
/// Computed for *all* children, before any of them are dropped from the
/// listing. Selecting first and measuring afterwards is what made a small
/// directory that doubled invisible behind a thousand large ones that did
/// nothing.
pub struct WindowDeltas {
    /// Total change over the window, indexed as `kids` was passed in.
    totals: Vec<i64>,
    /// Per scan, one entry per event that landed under a child.
    ///
    /// Appended rather than summed per (scan, child): a hash lookup per event
    /// measured a full second slower on a window holding 1.3M of them, and
    /// the list is walked exactly once afterwards.
    by_scan: std::collections::HashMap<ScanId, Vec<(u32, i64)>>,
    /// Change to the directory's own files, which belong to no child.
    own_total: i64,
    /// Every scan in the window, in order — the sparkline's x positions.
    scan_ids: Vec<ScanId>,
}

#[allow(clippy::too_many_arguments)]
fn window_deltas(
    s: &AppState,
    root: RootId,
    snap: &Snapshot,
    node: u32,
    kids: &[u32],
    s1: ScanId,
    s2: ScanId,
    metric: Metric,
) -> anyhow::Result<WindowDeltas> {
    let store = s.read();

    // One downward pass over the subtree, after which resolving an event is
    // an array index rather than a climb.
    let bands = band_map(snap, node, kids);
    // Kept only for paths no longer in the tree: a directory deleted
    // mid-window emits its event and then vanishes from the end-of-window
    // snapshot, so there is no index to look up and the climb has to reach
    // the database. Bounded by deletions, not by entities.
    let mut band_of: std::collections::HashMap<PathId, usize> = Default::default();
    for (bi, &k) in kids.iter().enumerate() {
        band_of.insert(snap.ids[k as usize], bi);
    }

    // Climbing from each event's path, rather than pre-loading the whole
    // dictionary. A directory deleted mid-window still has to resolve — it
    // emits a large negative event and then vanishes from the tree — and
    // `Ancestry` does not filter on liveness for exactly that reason.
    let mut ancestry = Ancestry::new(&store.conn);
    let node_id = snap.ids[node as usize];

    let mut st = store.conn.prepare(&format!(
        "SELECT scan_id FROM scan
         WHERE root_id = ?1 AND scan_id >= ?2 AND scan_id <= ?3 AND {USABLE_SCAN}
         ORDER BY scan_id"
    ))?;
    let scan_ids: Vec<ScanId> = st
        .query_map([root, s1, s2], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut ev = store.conn.prepare(
        "SELECT e.scan_id, e.path_id, e.d_bytes, e.d_blocks
         FROM size_event e JOIN path p ON p.path_id = e.path_id
         WHERE p.root_id = ?1 AND e.scan_id > ?2 AND e.scan_id <= ?3",
    )?;
    let mut by_scan: std::collections::HashMap<ScanId, Vec<(u32, i64)>> = Default::default();
    let rows = ev.query_map([root, s1, s2], |r| {
        Ok((
            r.get::<_, ScanId>(0)?,
            r.get::<_, PathId>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    // Totals across the whole window, per child, plus the viewed directory's
    // own files. These turn "size now" into "size then" without reading the
    // tree as it stood then.
    let mut totals = vec![0i64; kids.len()];
    let mut own_total = 0i64;
    for row in rows {
        let (sid, pid, db, dk) = row?;
        let d = if metric == Metric::Allocated { dk } else { db };
        if d == 0 {
            continue;
        }
        // An event on the directory itself moves its own files, not any
        // child's, and must not be attributed to one.
        if pid == node_id {
            own_total += d;
            continue;
        }
        // Which listed child holds this path — one array index when the path
        // still exists, a database climb only when it does not.
        let band = match snap.idx(pid) {
            Some(i) => match bands[i as usize] {
                NO_BAND | OTHER_BAND => continue,
                b => b as usize,
            },
            None => match ancestry.band(&band_of, node_id, pid)? {
                Climb::Band(b) => b,
                _ => continue,
            },
        };
        totals[band] += d;
        by_scan.entry(sid).or_default().push((band as u32, d));
    }

    Ok(WindowDeltas { totals, by_scan, own_total, scan_ids })
}

/// Downsampled value series for a chosen subset of the children.
///
/// `pick` names which entries of the original `kids` to plot; `level` arrives
/// holding each of those at its size *now* and is wound back to the start of
/// the window before the replay.
fn series_for(
    pick: &[usize],
    w: &WindowDeltas,
    level: &mut [i64],
    points: usize,
) -> Vec<Vec<i64>> {
    // Original child index -> position among the picked ones, as a lookup
    // table rather than a map: this is consulted once per event, and a window
    // containing a baseline scan holds one event per entity.
    const UNPICKED: u32 = u32::MAX;
    let n_children = w.totals.len();
    let mut pos = vec![UNPICKED; n_children];
    for (p, &i) in pick.iter().enumerate() {
        pos[i] = p as u32;
    }
    for (p, &i) in pick.iter().enumerate() {
        level[p] -= w.totals[i];
    }

    let n = w.scan_ids.len().max(1);
    let mut out: Vec<Vec<i64>> = vec![Vec::with_capacity(points.min(n)); pick.len()];
    let mut last_bucket = usize::MAX;
    for (i, sid) in w.scan_ids.iter().enumerate() {
        if let Some(deltas) = w.by_scan.get(sid) {
            for &(b, d) in deltas {
                let p = pos[b as usize];
                if p != UNPICKED {
                    level[p as usize] += d;
                }
            }
        }
        // Record once per bucket, plus always the final sample so the
        // sparkline's right-hand end is the current value rather than
        // whatever the last bucket boundary happened to be.
        let bucket = i * points / n;
        if bucket != last_bucket || i + 1 == w.scan_ids.len() {
            last_bucket = bucket;
            for (b, o) in out.iter_mut().enumerate() {
                o.push(level[b]);
            }
        }
    }
    out
}

/// What the snapshot cache is holding. Unauthenticated: it is a memory
/// figure, not data about anyone's files.
async fn cache_status(State(s): State<Arc<AppState>>) -> ApiResult {
    blocking(move || Ok(s.cache_stats())).await
}
