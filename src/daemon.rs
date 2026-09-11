//! The long-running service: a scan scheduler and the HTTP server.
//!
//! Scheduling lives in-process rather than in a systemd timer because the
//! daemon keeps the dictionary and snapshot cache warm between scans; a timer
//! firing a fresh process would throw that away every time.

use crate::api::AppState;
use crate::config::Config;
use crate::scan::walker::{ScanOptions, scan};
use crate::store::commit::{CommitOptions, commit_scan};
use anyhow::{Context, Result};
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
        .layer(access_log_layer(cfg.access_log));

    let listener = tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| {
        format!("binding {} (is another duTime already running?)", cfg.listen)
    })?;
    let bound = listener.local_addr()?;
    tracing::info!("duTime listening on http://{bound}");
    if cfg.access_log {
        tracing::info!("access log on: one line per HTTP request");
    }

    // Tell systemd we are actually up. The unit declares Type=notify, so
    // without this systemd waits for a readiness signal that never arrives
    // and eventually kills a service that was working perfectly.
    notify_ready(&cfg);
    spawn_watchdog();
    spawn_reachability_probe(bound);
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

/// One log line per HTTP request, when asked for.
///
/// The default `TraceLayer` emits at DEBUG, which the default filter hides —
/// so on a machine where the page will not load, the log shows a healthy
/// service and nothing else. This makes the traffic visible on demand, and
/// deliberately logs on *arrival* as well as on completion: a request that is
/// logged received but never logged answered is a very different bug from one
/// that never appears at all.
fn access_log_layer(
    on: bool,
) -> tower_http::trace::TraceLayer<
    tower_http::classify::SharedClassifier<tower_http::classify::ServerErrorsAsFailures>,
    tower_http::trace::DefaultMakeSpan,
    AccessLog,
    AccessLog,
> {
    tower_http::trace::TraceLayer::new_for_http()
        .on_request(AccessLog(on))
        .on_response(AccessLog(on))
}

#[derive(Clone, Copy)]
pub struct AccessLog(bool);

impl<B> tower_http::trace::OnRequest<B> for AccessLog {
    fn on_request(&mut self, req: &axum::http::Request<B>, _: &tracing::Span) {
        if self.0 {
            tracing::info!("--> {} {}", req.method(), req.uri());
        }
    }
}

impl<B> tower_http::trace::OnResponse<B> for AccessLog {
    fn on_response(self, res: &axum::http::Response<B>, latency: Duration, _: &tracing::Span) {
        if self.0 {
            tracing::info!("<-- {} in {:.1?}", res.status().as_u16(), latency);
        }
    }
}

/// Check, once, whether we are reachable from off this machine.
///
/// duTime's own system unit sets `IPAddressAllow=localhost`, so setting
/// `listen = "0.0.0.0:8471"` and nothing else produces a service that binds
/// successfully, logs that it is listening, passes every health check a local
/// operator can run — and drops every packet from the browser that is trying
/// to reach it. Nothing in the system reports this, because dropping a packet
/// is not an error anyone gets told about. So we go and look.
///
/// Runs after readiness and off the startup path: when the answer is bad, the
/// probe is slow by definition, and a diagnostic must never be the reason a
/// service is marked as failing to start.
fn spawn_reachability_probe(bound: std::net::SocketAddr) {
    let Some(target) = crate::diag::external_target(bound) else {
        let p = bound.port();
        tracing::info!(
            "bound to loopback: reachable only from this machine. To reach it from \
             elsewhere, tunnel it (ssh -N -L {p}:localhost:{p} <this-host>) or set \
             listen = \"0.0.0.0:{p}\" — which on a system install also needs \
             IPAddressAllow= widening in the unit; `dutime install --listen` does both."
        );
        return;
    };
    tokio::task::spawn_blocking(move || {
        let r = crate::diag::probe(target, Duration::from_secs(3));
        if r.ok() {
            // Deliberately not "reachable": this probe leaves from inside our
            // own cgroup and loops back without touching the wire, so it
            // clears systemd's filter but says nothing about a host firewall.
            tracing::info!("self-check: {target} accepts connections (a host firewall is still untested)");
        } else {
            tracing::warn!(
                "self-check FAILED: cannot connect to my own listen address {target} — {}",
                r.advice()
            );
            tracing::warn!("run `dutime doctor` for the full check");
        }
    });
}
