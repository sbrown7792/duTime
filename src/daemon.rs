//! The long-running service: a scan scheduler and the HTTP server.
//!
//! Scheduling lives in-process rather than in a systemd timer because the
//! daemon keeps the dictionary and snapshot cache warm between scans; a timer
//! firing a fresh process would throw that away every time.

use crate::api::AppState;
use crate::config::Config;
use crate::scan::walker::{ScanOptions, scan};
use crate::store::commit::{CommitOptions, commit_scan};
use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Consecutive overruns before the effective interval is doubled.
const OVERRUNS_BEFORE_BACKOFF: u32 = 3;
/// Never back off beyond this multiple of the configured interval.
const MAX_BACKOFF: u32 = 8;

pub struct Scheduler {
    state: Arc<AppState>,
    cfg: Config,
}

impl Scheduler {
    pub fn new(state: Arc<AppState>, cfg: Config) -> Self {
        Self { state, cfg }
    }

    /// One task per root, each with its own interval and its own backoff.
    pub fn spawn_all(self: Arc<Self>) {
        for root in self.cfg.roots.clone() {
            let me = self.clone();
            tokio::spawn(async move { me.run_root(root).await });
        }
    }

    async fn run_root(&self, root: crate::config::RootConfig) {
        // A lock per root, not a queue. If a scan is still running when the
        // next tick fires, the tick is dropped: queueing turns one slow scan
        // into a backlog that never drains and eventually thrashes the disk
        // it was supposed to be monitoring quietly.
        let running = Arc::new(AtomicBool::new(false));
        let mut overruns: u32 = 0;
        let mut backoff: u32 = 1;

        if self.cfg.scan_on_start {
            self.scan_once(&root, &running).await;
        }

        loop {
            let wait = root.interval_s.max(1) * backoff as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;

            if running.load(Ordering::SeqCst) {
                overruns += 1;
                tracing::warn!(
                    root = %root.path.display(),
                    overruns,
                    "scan still running when the next one was due; skipping this tick"
                );
                if overruns >= OVERRUNS_BEFORE_BACKOFF && backoff < MAX_BACKOFF {
                    backoff *= 2;
                    tracing::warn!(
                        root = %root.path.display(),
                        new_interval_s = root.interval_s * backoff as u64,
                        "scans are slower than the interval; backing off"
                    );
                }
                continue;
            }

            if self.scan_once(&root, &running).await {
                // Recover as soon as scans fit in the interval again.
                overruns = 0;
                if backoff > 1 {
                    backoff = 1;
                    tracing::info!(root = %root.path.display(), "scan times recovered; interval restored");
                }
            }
        }
    }

    async fn scan_once(&self, root: &crate::config::RootConfig, running: &Arc<AtomicBool>) -> bool {
        running.store(true, Ordering::SeqCst);
        let res = self.do_scan(root).await;
        running.store(false, Ordering::SeqCst);
        match res {
            Ok(()) => true,
            Err(e) => {
                tracing::error!(root = %root.path.display(), "scan failed: {e:#}");
                false
            }
        }
    }

    async fn do_scan(&self, root: &crate::config::RootConfig) -> Result<()> {
        let mut opts = ScanOptions::new(&root.path);
        opts.track_file_min_bytes = root.track_file_min_bytes;
        opts.one_filesystem = root.one_filesystem;
        opts.exclude = root.exclude.clone();
        opts.exclude_prefixes = root.exclude_paths.clone();
        opts.threads = self.cfg.threads;

        let started_at = crate::cli::now();
        let t0 = Instant::now();

        // The walk is blocking and syscall-heavy; keep it off the async
        // runtime so the UI stays responsive while it runs.
        let result = tokio::task::spawn_blocking(move || {
            let r = scan(&opts)?;
            let roll = r.tree.rollup();
            anyhow::Ok((r, roll, opts))
        })
        .await??;
        let (r, roll, opts) = result;
        let walk_ms = t0.elapsed().as_millis() as i64;

        let state = self.state.clone();
        let canon = opts.root.canonicalize().unwrap_or(opts.root.clone());
        let cops = CommitOptions {
            checkpoint_every_scans: self.cfg.checkpoint_every_scans,
            checkpoint_min_bytes: self.cfg.checkpoint_min_bytes,
        };
        let stats = tokio::task::spawn_blocking(move || {
            let mut store = state.store.lock().unwrap();
            let root_id = store.ensure_root(&canon)?;
            commit_scan(
                &mut store, root_id, &canon, &r.tree, &roll, &r.stats, started_at, walk_ms, &cops,
            )
        })
        .await??;

        tracing::info!(
            root = %root.path.display(),
            scan = stats.scan_id,
            events = stats.n_events,
            born = stats.n_born,
            gone = stats.n_gone + stats.n_subtree_gone,
            walk_ms,
            "scan complete"
        );
        Ok(())
    }
}

pub async fn serve(cfg: Config) -> Result<()> {
    // A writer plus a pool of readers, so the web UI never queues behind the
    // scanner's commit.
    let state = Arc::new(AppState::open(&cfg.db)?);
    {
        let store = state.store.lock().unwrap();
        for r in &cfg.roots {
            let canon = r.path.canonicalize().unwrap_or_else(|_| r.path.clone());
            store.ensure_root(&canon)?;
        }
    }

    let sched = Arc::new(Scheduler::new(state.clone(), cfg.clone()));
    sched.spawn_all();

    let app = crate::api::router(state)
        .fallback(crate::web::serve)
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(cfg.listen).await?;
    tracing::info!("duTime listening on http://{}", listener.local_addr()?);

    // Tell systemd we are actually up. The unit declares Type=notify, so
    // without this systemd waits for a readiness signal that never arrives
    // and eventually kills a service that was working perfectly.
    notify_ready(&cfg);
    spawn_watchdog();
    for r in &cfg.roots {
        tracing::info!(
            "  tracking {} every {}",
            r.path.display(),
            humantime::format_duration(Duration::from_secs(r.interval_s))
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

/// Signal readiness, and describe what we are doing in `systemctl status`.
fn notify_ready(cfg: &Config) {
    use sd_notify::NotifyState;
    let status = format!(
        "serving on {}; tracking {} root(s)",
        cfg.listen,
        cfg.roots.len()
    );
    // Harmless no-op when not running under systemd.
    let _ = sd_notify::notify(&[NotifyState::Ready, NotifyState::Status(&status)]);
}

/// Keep the systemd watchdog fed, if one is configured.
///
/// The unit sets WatchdogSec, so a hung process gets restarted — but only if
/// we actually ping. Ping at half the configured interval, which is the
/// margin systemd's own documentation recommends.
fn spawn_watchdog() {
    let Some(timeout) = sd_notify::watchdog_enabled() else { return };
    let period = timeout / 2;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(period).await;
            let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
        }
    });
}

async fn shutdown() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
    let term = async {
        #[cfg(unix)]
        {
            let mut s = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
            s.recv().await;
        }
        #[cfg(not(unix))]
        std::future::pending::<()>().await;
    };
    tokio::select! { _ = ctrl_c => {}, _ = term => {} }
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
    tracing::info!("shutting down");
}
