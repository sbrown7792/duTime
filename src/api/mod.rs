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
use crate::store::query::{self, Extreme};
use crate::store::snapshot::Snapshot;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
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
        .with_state(state)
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
    fn pick_root(&self, want: Option<RootId>) -> anyhow::Result<RootId> {
        let store = self.read();
        let roots = store.roots()?;
        match want {
            Some(r) if roots.iter().any(|(id, _)| *id == r) => Ok(r),
            Some(r) => anyhow::bail!("no such root: {r}"),
            None => roots
                .first()
                .map(|(id, _)| *id)
                .ok_or_else(|| anyhow::anyhow!("no roots tracked yet — run a scan first")),
        }
    }

    /// Resolve a time expression to a concrete scan.
    fn pick_scan(&self, root: RootId, at: &Option<String>) -> anyhow::Result<(ScanId, i64)> {
        let now = crate::cli::now();
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
        Ok(json!({
            "status": "ok",
            "version": env!("CARGO_PKG_VERSION"),
            "roots": roots.len(),
        }))
    })
    .await
}

async fn roots(State(s): State<Arc<AppState>>) -> ApiResult {
    blocking(move || {
        let store = s.read();
        let mut out = Vec::new();
        for (id, path) in store.roots()? {
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

async fn scans(State(s): State<Arc<AppState>>, Query(q): Query<ScansQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
        let store = s.read();
        let mut st = store.conn.prepare(
            "SELECT scan_id, started_at, duration_ms, n_events, incl_bytes, incl_blocks,
                    n_dirs, n_files, fs_total, fs_free, fs_avail
             FROM scan WHERE root_id = ?1 AND status = 'ok'
             ORDER BY scan_id DESC LIMIT ?2",
        )?;
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

async fn resolve(State(s): State<Arc<AppState>>, Query(q): Query<Common>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
        let (scan_id, at) = s.pick_scan(root, &q.at)?;
        Ok(json!({ "scan_id": scan_id, "at": at }))
    })
    .await
}

/// Everything the landing page needs, in one round trip.
async fn overview(State(s): State<Arc<AppState>>, Query(q): Query<Common>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
        let metric = metric_of(&q.metric);
        let (scan_id, at) = s.pick_scan(root, &q.at)?;

        let (path, first, total_scans, fs, history) = {
            let store = s.read();
            let path = store.root_path(root)?;
            let first = store.first_scan(root)?;
            let total = store.scan_count(root)?;
            let fs: (Option<i64>, Option<i64>, Option<i64>) = store.conn.query_row(
                "SELECT fs_total, fs_free, fs_avail FROM scan WHERE scan_id = ?1",
                [scan_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            let mut st = store.conn.prepare(
                "SELECT started_at, incl_bytes, incl_blocks, fs_free FROM scan
                 WHERE root_id = ?1 AND status = 'ok' ORDER BY scan_id",
            )?;
            let rows = st.query_map([root], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?;
            let history: Vec<(i64, i64, i64, Option<i64>)> = rows.collect::<rusqlite::Result<_>>()?;
            (path, first, total, fs, history)
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
async fn tree(State(s): State<Arc<AppState>>, Query(q): Query<TreeQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
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
async fn diff(State(s): State<Arc<AppState>>, Query(q): Query<DiffQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
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
async fn series(State(s): State<Arc<AppState>>, Query(q): Query<SeriesQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
        let metric = metric_of(&q.metric);
        let (s2, at2) = s.pick_scan(root, &q.to)?;
        let (s1, at1) = s.pick_scan(root, &Some(q.from.clone().unwrap_or_else(|| "-7d".into())))
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
        let mut band_of: std::collections::HashMap<PathId, usize> = Default::default();
        for (bi, &k) in bands.iter().enumerate() {
            band_of.insert(snap.ids[k as usize], bi);
        }
        const OTHER: usize = usize::MAX - 1;
        const SELF_BAND: usize = usize::MAX;

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
        let mut parent_of: std::collections::HashMap<PathId, Option<PathId>> = Default::default();
        {
            let mut ps = store.conn.prepare(
                "SELECT path_id, parent_id FROM path
                 WHERE root_id = ?1 AND born_scan <= ?2
                   AND (died_scan IS NULL OR died_scan > ?3)",
            )?;
            let rows = ps.query_map([root, s2, s1], |r| {
                Ok((r.get::<_, PathId>(0)?, r.get::<_, Option<PathId>>(1)?))
            })?;
            for row in rows {
                let (id, par) = row?;
                parent_of.insert(id, par);
            }
        }
        let node_id = snap.ids[node as usize];

        // Starting value for each band, plus the directory's own files.
        let mut level: Vec<i64> = Vec::with_capacity(bands.len() + 2);
        for &k in &bands {
            level.push(query::incl_at(&store, snap.ids[k as usize], s1)?.0);
        }
        let node_total_start = query::incl_at(&store, snap.ids[node as usize], s1)?.0;
        let kids_start: i64 = level.iter().sum();
        let other_and_own = node_total_start - kids_start;
        level.push(other_and_own.max(0)); // "everything else"

        // Every scan in the window becomes an x position.
        let mut st = store.conn.prepare(
            "SELECT scan_id, started_at FROM scan
             WHERE root_id = ?1 AND scan_id >= ?2 AND scan_id <= ?3 AND status = 'ok'
             ORDER BY scan_id",
        )?;
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
            // Walk up until we hit a band, the node itself, or leave the subtree.
            let mut cur = pid;
            let mut band = None;
            loop {
                if let Some(&b) = band_of.get(&cur) {
                    band = Some(b);
                    break;
                }
                if cur == node_id {
                    band = Some(SELF_BAND);
                    break;
                }
                match parent_of.get(&cur) {
                    Some(Some(p)) => cur = *p,
                    // Either the root, or an ancestor outside the window.
                    _ => break,
                }
            }
            let slot = match band {
                Some(SELF_BAND) => OTHER,
                Some(b) => b,
                None => continue, // outside this subtree entirely
            };
            by_scan.entry(sid).or_default().push((slot, d));
        }

        let n_bands = level.len();
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

async fn gainers(State(s): State<Arc<AppState>>, Query(q): Query<GainersQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
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
async fn listing(State(s): State<Arc<AppState>>, Query(q): Query<ListingQ>) -> ApiResult {
    blocking(move || {
        let root = s.pick_root(q.root)?;
        let metric = metric_of(&q.metric);
        let (s2, at2) = s.pick_scan(root, &q.at)?;

        // Clamp to the first scan rather than reaching past the start of
        // history, which would report the baseline's births as growth.
        let (s1, at1, clamped) = {
            let store = s.read();
            let now = crate::cli::now();
            let want = match crate::cli::timespec::parse(
                q.from.as_deref().unwrap_or("-7d"),
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
                Some((id, at)) if id <= s2 => (id, at, false),
                _ => {
                    let (id, at) = store
                        .first_scan(root)?
                        .ok_or_else(|| anyhow::anyhow!("no scans recorded"))?;
                    (id, at, true)
                }
            }
        };

        let snap = s.snapshot(root, s2)?;
        let base = s.snapshot(root, s1)?;
        let node = locate(&snap, q.path.as_deref())?;
        let limit = q.limit.unwrap_or(500).min(5000);
        let points = q.points.unwrap_or(32).clamp(2, 240);

        let size_of = |sn: &Snapshot, i: u32| -> i64 {
            match metric {
                Metric::Allocated => sn.incl_blocks[i as usize],
                Metric::Apparent => sn.incl_bytes[i as usize],
            }
        };

        let mut kids: Vec<u32> = snap.children[node as usize].clone();
        kids.sort_by_key(|&c| std::cmp::Reverse(size_of(&snap, c)));
        kids.truncate(limit);

        // Seed from the snapshot at the window's start rather than a query
        // per child: snapshots are cached, so this is a lookup each.
        let mut level: Vec<i64> = kids
            .iter()
            .map(|&c| base.idx(snap.ids[c as usize]).map(|i| size_of(&base, i)).unwrap_or(0))
            .collect();

        let spark = sparklines(&s, root, &snap, node, &kids, s1, s2, metric, &mut level, points)?;

        let own_now = match metric {
            Metric::Allocated => snap.own_blocks[node as usize],
            Metric::Apparent => snap.own_bytes[node as usize],
        };
        let own_then = base
            .idx(snap.ids[node as usize])
            .map(|i| match metric {
                Metric::Allocated => base.own_blocks[i as usize],
                Metric::Apparent => base.own_bytes[i as usize],
            })
            .unwrap_or(0);

        let rows: Vec<Value> = kids
            .iter()
            .enumerate()
            .map(|(bi, &c)| {
                let i = c as usize;
                let now = size_of(&snap, c);
                let then = base
                    .idx(snap.ids[i])
                    .map(|k| size_of(&base, k))
                    .unwrap_or(0);
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
            "truncated": snap.children[node as usize].len().saturating_sub(kids.len()),
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
/// `level` arrives holding each child's size at the start of the window and is
/// advanced in place as the window's events are replayed.
#[allow(clippy::too_many_arguments)]
fn sparklines(
    s: &AppState,
    root: RootId,
    snap: &Snapshot,
    node: u32,
    kids: &[u32],
    s1: ScanId,
    s2: ScanId,
    metric: Metric,
    level: &mut [i64],
    points: usize,
) -> anyhow::Result<Vec<Vec<i64>>> {
    let store = s.read();

    let mut band_of: std::collections::HashMap<PathId, usize> = Default::default();
    for (bi, &k) in kids.iter().enumerate() {
        band_of.insert(snap.ids[k as usize], bi);
    }

    // Parent links covering everything alive at any point in the window.
    //
    // The end-of-window snapshot is not enough: a directory deleted mid-window
    // emits its large negative event and then vanishes from the tree, so its
    // delta would be dropped and the child it belonged to would keep bytes
    // that no longer exist.
    let mut parent_of: std::collections::HashMap<PathId, Option<PathId>> = Default::default();
    {
        let mut ps = store.conn.prepare(
            "SELECT path_id, parent_id FROM path
             WHERE root_id = ?1 AND born_scan <= ?2
               AND (died_scan IS NULL OR died_scan > ?3)",
        )?;
        let rows = ps.query_map([root, s2, s1], |r| {
            Ok((r.get::<_, PathId>(0)?, r.get::<_, Option<PathId>>(1)?))
        })?;
        for row in rows {
            let (id, par) = row?;
            parent_of.insert(id, par);
        }
    }
    let node_id = snap.ids[node as usize];

    let mut st = store.conn.prepare(
        "SELECT scan_id FROM scan
         WHERE root_id = ?1 AND scan_id >= ?2 AND scan_id <= ?3 AND status = 'ok'
         ORDER BY scan_id",
    )?;
    let scan_ids: Vec<ScanId> = st
        .query_map([root, s1, s2], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut ev = store.conn.prepare(
        "SELECT e.scan_id, e.path_id, e.d_bytes, e.d_blocks
         FROM size_event e JOIN path p ON p.path_id = e.path_id
         WHERE p.root_id = ?1 AND e.scan_id > ?2 AND e.scan_id <= ?3",
    )?;
    let mut by_scan: std::collections::HashMap<ScanId, Vec<(usize, i64)>> = Default::default();
    let rows = ev.query_map([root, s1, s2], |r| {
        Ok((
            r.get::<_, ScanId>(0)?,
            r.get::<_, PathId>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
        ))
    })?;
    for row in rows {
        let (sid, pid, db, dk) = row?;
        let d = if metric == Metric::Allocated { dk } else { db };
        if d == 0 {
            continue;
        }
        // Climb until we land on one of the listed children.
        let mut cur = pid;
        let mut band = None;
        loop {
            if let Some(&b) = band_of.get(&cur) {
                band = Some(b);
                break;
            }
            if cur == node_id {
                break; // the directory's own files, not any child
            }
            match parent_of.get(&cur) {
                Some(Some(p)) => cur = *p,
                _ => break,
            }
        }
        if let Some(b) = band {
            by_scan.entry(sid).or_default().push((b, d));
        }
    }

    let n = scan_ids.len().max(1);
    let mut out: Vec<Vec<i64>> = vec![Vec::with_capacity(points.min(n)); kids.len()];
    let mut last_bucket = usize::MAX;
    for (i, sid) in scan_ids.iter().enumerate() {
        if let Some(deltas) = by_scan.get(sid) {
            for &(b, d) in deltas {
                level[b] += d;
            }
        }
        // Record once per bucket, plus always the final sample so the
        // sparkline's right-hand end is the current value rather than
        // whatever the last bucket boundary happened to be.
        let bucket = i * points / n;
        if bucket != last_bucket || i + 1 == scan_ids.len() {
            last_bucket = bucket;
            for (b, o) in out.iter_mut().enumerate() {
                o.push(level[b]);
            }
        }
    }
    Ok(out)
}
