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
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Set once when a signal arrives, watched by every in-flight walk.
///
/// Shared rather than global so a scan started by anything else — the CLI,
/// a test — is unaffected, and so there is no per-scan task polling it.
///
/// A walk is a blocking task, and a tokio runtime does not finish dropping
/// until its blocking tasks return — so before this existed, a restart during
/// an hour-long walk waited for the walk. Measured on a real server: the
/// `/media/nextcloud` walk runs a median of 80 minutes, against a systemd
/// stop timeout of 90 seconds, so every such restart ended in SIGKILL.
///
/// A commit is deliberately *not* cancellable. It takes seconds rather than
/// minutes (measured: 6.2 s for 1.3M entities, 2.6 s in steady state), and
/// interrupting one throws away a walk that has already finished.
/// Consecutive overruns before the effective interval is doubled.
const OVERRUNS_BEFORE_BACKOFF: u32 = 3;
/// Never back off beyond this multiple of the configured interval.
const MAX_BACKOFF: u32 = 8;

pub struct Scheduler {
    state: Arc<AppState>,
    cfg: Config,
    cancel: Arc<AtomicBool>,
}

impl Scheduler {
    pub fn new(state: Arc<AppState>, cfg: Config, cancel: Arc<AtomicBool>) -> Self {
        Self { state, cfg, cancel }
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
        // Mounts under this root that the walk will not cross. Announced when
        // first seen and whenever the set changes, not every hour: skipping
        // another filesystem is correct behaviour, but it silently leaves its
        // bytes out of the total, so it must be said once rather than never.
        let announced: Arc<Mutex<Option<Vec<PathBuf>>>> = Arc::new(Mutex::new(None));
        let mut overruns: u32 = 0;
        let mut backoff: u32 = 1;

        // The path the diagnostics view will join on. Canonical, because that
        // is the form the store records and the form the walk resolves to.
        let key = root.path.canonicalize().unwrap_or_else(|_| root.path.clone());
        self.state.note_activity(&key, |a| {
            a.interval_s = root.interval_s.max(1);
            a.effective_interval_s = a.interval_s;
        });

        if self.cfg.scan_on_start {
            self.scan_once(&root, &key, &running, &announced).await;
        }

        loop {
            let wait = root.interval_s.max(1) * backoff as u64;
            self.state.note_activity(&key, |a| {
                a.effective_interval_s = wait;
                a.next_due = Some(crate::cli::now() + wait as i64);
            });
            tokio::time::sleep(Duration::from_secs(wait)).await;

            if running.load(Ordering::SeqCst) {
                overruns += 1;
                self.state.note_activity(&key, |a| a.overruns = overruns);
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

            if self.scan_once(&root, &key, &running, &announced).await {
                // Recover as soon as scans fit in the interval again.
                overruns = 0;
                self.state.note_activity(&key, |a| a.overruns = 0);
                if backoff > 1 {
                    backoff = 1;
                    tracing::info!(root = %root.path.display(), "scan times recovered; interval restored");
                }
            }
        }
    }

    async fn scan_once(
        &self,
        root: &crate::config::RootConfig,
        key: &Path,
        running: &Arc<AtomicBool>,
        announced: &Arc<Mutex<Option<Vec<PathBuf>>>>,
    ) -> bool {
        running.store(true, Ordering::SeqCst);
        let began = crate::cli::now();
        let t0 = Instant::now();
        self.state.note_activity(key, |a| {
            a.running_since = Some(began);
            a.last_started = Some(began);
        });
        let res = self.do_scan(root, announced).await;
        running.store(false, Ordering::SeqCst);
        let took = t0.elapsed().as_millis() as i64;
        match res {
            Ok(()) => {
                self.state.note_activity(key, |a| {
                    a.running_since = None;
                    a.last_walk_ms = Some(took);
                    a.last_error = None;
                    a.last_error_disk_full = false;
                    a.consecutive_failures = 0;
                    a.scans_completed += 1;
                });
                true
            }
            Err(e) => {
                tracing::error!(root = %root.path.display(), "scan failed: {e:#}");
                // Kept verbatim: a diagnostics page that says a scan failed
                // without saying why sends you to the journal anyway.
                let full = crate::store::ballast::is_disk_full(&e);
                self.state.note_activity(key, |a| {
                    a.running_since = None;
                    a.last_walk_ms = Some(took);
                    a.last_error = Some(format!("{e:#}"));
                    a.last_error_disk_full = full;
                    a.scans_failed += 1;
                    a.consecutive_failures += 1;
                });
                false
            }
        }
    }

    async fn do_scan(
        &self,
        root: &crate::config::RootConfig,
        announced: &Arc<Mutex<Option<Vec<PathBuf>>>>,
    ) -> Result<()> {
        let mut opts = ScanOptions::new(&root.path);
        opts.track_file_min_bytes = root.track_file_min_bytes;
        opts.one_filesystem = root.one_filesystem;
        opts.exclude = root.exclude.clone();
        opts.exclude_prefixes = root.exclude_paths.clone();
        opts.threads = self.cfg.threads;
        opts.cancel = Some(self.cancel.clone());

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

        // A walk that stopped early has seen a fragment of the tree. Its
        // totals are not small because the disk emptied, they are small
        // because we stopped looking — and a scan row saying so would show
        // up as a cliff on every chart. Drop it and let the next scan,
        // after the restart, measure the tree properly.
        if r.stats.cancelled {
            tracing::info!(
                root = %root.path.display(),
                walk_ms,
                "shutting down mid-walk; discarding the partial scan"
            );
            return Ok(());
        }
        // Read before `r` moves into the commit closure.
        let n_errors = r.stats.n_errors;
        let examples = r.stats.unreadable.clone();
        let skipped = r.stats.skipped_mounts.clone();
        let other_fs = r.stats.other_filesystems.clone();
        let root_fstype = r.stats.root_fstype.clone();
        let n_entities = r.tree.len();
        let n_dirs = r.stats.n_dirs;

        let state = self.state.clone();
        let db_path = self.cfg.db.clone();
        let canon = opts.root.canonicalize().unwrap_or(opts.root.clone());
        let cops = CommitOptions {
            checkpoint_every_scans: self.cfg.checkpoint_every_scans,
            checkpoint_min_bytes: self.cfg.checkpoint_min_bytes,
        };
        // The commit that matters most is the one taken as the disk fills,
        // and that is the one with no room to be written. Spending the
        // reserve buys it back — see `store::ballast`.
        let ballast =
            crate::store::ballast::Ballast::beside(&self.cfg.db, self.cfg.ballast_bytes);
        let stats = tokio::task::spawn_blocking(move || {
            let mut store = state.store.lock().unwrap();
            let root_id = store.ensure_root(&canon)?;
            crate::store::ballast::with_rescue(&db_path, "committing a scan", || {
                commit_scan(
                    &mut store, root_id, &canon, &r.tree, &roll, &r.stats, started_at, walk_ms,
                    &cops,
                )
            })
        })
        .await??;
        // Re-arm for next time. `ensure` declines while the disk is still
        // nearly full, so this cannot be what keeps it full.
        if let Err(e) = ballast.ensure() {
            tracing::debug!("could not re-reserve disk space: {e:#}");
        }

        tracing::info!(
            root = %root.path.display(),
            scan = stats.scan_id,
            events = stats.n_events,
            born = stats.n_born,
            gone = stats.n_gone + stats.n_subtree_gone,
            walk_ms,
            "scan complete"
        );
        // Everything under an unreadable directory is simply absent from the
        // total, so a quiet scan here would report a shrink that never
        // happened. Warn, and name paths: the fix is a permission, and you
        // cannot grant a permission to a count.
        // Announce the *other filesystems* once, and again if the set
        // changes. Deliberately not the fstype-denied mounts: a scan of /
        // skips 89 of those — every snap image and every virtual filesystem —
        // and dumping them hourly would train anyone reading the log to skip
        // past exactly the place a real finding appears. Their count goes in
        // the same line; the paths are a debug-level detail.
        {
            let mut seen = announced.lock().unwrap();
            if seen.as_deref() != Some(other_fs.as_slice()) {
                if !other_fs.is_empty() {
                    tracing::info!(
                        root = %root.path.display(),
                        "not crossing {} filesystem(s) mounted under this root, so their \
                         bytes are NOT in its total: {}. Give one its own [[root]] to track \
                         it, or set one_filesystem = false to fold it in.",
                        other_fs.len(),
                        other_fs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                    );
                } else if seen.is_some() {
                    tracing::info!(
                        root = %root.path.display(),
                        "no separate filesystems are mounted under this root any more"
                    );
                }
                *seen = Some(other_fs.clone());
            }
        }
        if !skipped.is_empty() {
            tracing::debug!(
                root = %root.path.display(),
                "also skipped {} virtual or duplicate mount(s): {}",
                skipped.len(),
                skipped.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            );
        }

        explain_empty_scan(
            &root.path,
            n_entities,
            n_dirs,
            n_errors,
            &other_fs,
            root_fstype.as_deref(),
            &opts,
        );
        if n_errors > 0 {
            tracing::warn!(
                root = %root.path.display(),
                scan = stats.scan_id,
                unreadable = n_errors,
                "scan is PARTIAL — {n_errors} path(s) could not be read, so this total is \
                 lower than the truth. Examples: {}{}",
                examples.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", "),
                if n_errors > examples.len() as i64 { ", ..." } else { "" }
            );
            tracing::warn!(
                "{}",
                permission_advice_for(root_fstype.as_deref(), Some(&root.path))
            );
        }
        Ok(())
    }
}

pub async fn serve(cfg: Config) -> Result<()> {
    // A writer plus a pool of readers, so the web UI never queues behind the
    // scanner's commit.
    let mut state = AppState::open(&cfg.db)?;

    // Take the disk reserve before the first scan, so the protection is in
    // place from the moment the service is up rather than from its first
    // successful commit an interval later.
    let ballast = crate::store::ballast::Ballast::beside(&cfg.db, cfg.ballast_bytes);
    if let Err(e) = ballast.ensure() {
        tracing::warn!("could not reserve disk space beside the database: {e:#}");
    }
    state.set_ballast(cfg.ballast_bytes);

    // Roots get their ids on first sight, and the access policy is keyed by
    // id, so registration has to happen before the policy is installed.
    let mut protected = std::collections::HashSet::new();
    {
        let store = state.store.lock().unwrap();
        for r in &cfg.roots {
            let canon = r.path.canonicalize().unwrap_or_else(|_| r.path.clone());
            let id = store.ensure_root(&canon)?;
            if r.protected {
                protected.insert(id);
            }
        }
    }

    let auth = crate::auth::Auth::from_config(&cfg.auth)?;
    // A root marked protected with no token configured is not protected, and
    // the config says otherwise — which is worse than either, because
    // somebody has decided the problem is handled. Refuse to start.
    if !protected.is_empty() && auth.is_open() {
        anyhow::bail!(
            "{} root(s) are marked `protected` but no token is configured, so nothing \
             would actually be protected.\n\nFix with:\n  \
             sudo dutime token --write /etc/dutime/token\n\
             then add to the config:\n  [auth]\n  token_file = \"/etc/dutime/token\"",
            protected.len()
        );
    }
    if !auth.is_open() && protected.is_empty() {
        tracing::warn!(
            "a token is configured but no root is marked `protected`, so it is never \
             required. Add `protected = true` to the [[root]] blocks you want to gate."
        );
    }
    let n_protected = protected.len();
    state.set_access(auth, protected);
    let state = Arc::new(state);

    // Set by the signal handler, read by whatever walk is in flight.
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = Arc::new(Scheduler::new(state.clone(), cfg.clone(), cancel.clone()));
    sched.spawn_all();

    let app = crate::api::router(state)
        .fallback(crate::web::serve)
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(access_log_layer(cfg.access_log));

    let listener = tokio::net::TcpListener::bind(cfg.listen).await.with_context(|| {
        format!("binding {} (is another duTime already running?)", cfg.listen)
    })?;
    let bound = listener.local_addr()?;
    // Version first, and with the binary's own mtime: on a machine where the
    // deploy is "copy the binary over", the commonest reason a fix appears
    // not to work is that the fix is not there.
    tracing::info!(
        "duTime {} — binary built {}",
        crate::cli::VERSION,
        crate::cli::build_mtime().unwrap_or_else(|| "unknown".into())
    );
    tracing::info!("listening on http://{bound}");
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
            "  tracking {} every {}{}",
            r.path.display(),
            humantime::format_duration(Duration::from_secs(r.interval_s)),
            if r.protected { "  [protected: token required]" } else { "" }
        );
    }
    // The combination that quietly publishes a filesystem inventory to the
    // network: bound to every interface with nothing gated.
    if !bound.ip().is_loopback() && n_protected == 0 {
        tracing::warn!(
            "serving on {bound} with no protected roots — anyone who can reach this port \
             can read every filename and size duTime has recorded. Mark sensitive roots \
             with `protected = true` and set [auth] token_file, or bind 127.0.0.1."
        );
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(cancel))
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

async fn shutdown(cancel: Arc<AtomicBool>) {
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
    // Set before anything else: a walk in progress should start unwinding
    // while the HTTP server is still draining, not after.
    cancel.store(true, Ordering::SeqCst);
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

/// Say why a scan found nothing, at the moment it finds nothing.
///
/// "scan complete root=/media/nextcloud events=1 born=1" is a success message
/// describing a failure. One entity is the root directory and nothing else,
/// and the log line reads the same whether the volume is genuinely empty, was
/// unreadable, sits behind a mount the walk declined to cross, or was
/// excluded. Every one of those has a different fix, and none of them is
/// discoverable from that line.
///
/// So: when a scan of a whole volume comes back with almost nothing, print
/// the candidate reasons together with what this scan actually observed.
fn explain_empty_scan(
    root: &std::path::Path,
    n_entities: usize,
    n_dirs: i64,
    n_errors: i64,
    other_fs: &[std::path::PathBuf],
    root_fstype: Option<&str>,
    opts: &ScanOptions,
) {
    // A root holding only itself, or a couple of directories and no tracked
    // file, is the shape worth questioning. A genuinely empty volume trips
    // this too, and saying so once per scan of an empty volume is a much
    // smaller cost than the alternative.
    if n_entities > 4 && n_dirs > 2 {
        return;
    }

    tracing::warn!(
        root = %root.display(),
        entities = n_entities,
        "this scan found almost nothing ({n_entities} tracked entit{}). If that volume is \
         not actually empty, one of the following is why:",
        if n_entities == 1 { "y" } else { "ies" }
    );

    if n_errors > 0 {
        tracing::warn!("  - {n_errors} path(s) could not be read (see the warning below)");
        tracing::warn!("    {}", permission_advice_for(root_fstype, Some(root)));
    } else {
        // Worth stating explicitly: it removes the most-suspected cause.
        tracing::warn!(
            "  - not permissions: every path duTime tried was readable, so the capability \
             is working"
        );
    }

    if !other_fs.is_empty() {
        tracing::warn!(
            "  - THIS IS THE LIKELY ONE: {} separate filesystem(s) are mounted under this \
             root and were not crossed: {}. Give the one holding your data its own \
             [[root]], or set one_filesystem = false on this root.",
            other_fs.len(),
            other_fs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    } else if opts.one_filesystem {
        tracing::warn!(
            "  - not a nested mount: nothing under this root is on a different device, so \
             one_filesystem is not what is hiding it"
        );
    }

    // Count only the absolute excludes that fall under this root. The
    // defaults list /proc, /tmp and friends, and offering all nine as
    // suspects when none of them is inside this volume is a false lead.
    let applicable: Vec<&std::path::PathBuf> = opts
        .exclude_prefixes
        .iter()
        .filter(|p| p.starts_with(root) && p.as_path() != root)
        .collect();
    if !opts.exclude.is_empty() || !applicable.is_empty() {
        tracing::warn!(
            "  - {} exclude pattern(s){} are in force; check them with \
             `dutime config --check`",
            opts.exclude.len(),
            if applicable.is_empty() {
                String::new()
            } else {
                format!(
                    " and {} excluded path(s) inside this root ({})",
                    applicable.len(),
                    applicable.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                )
            }
        );
    }

    tracing::warn!(
        "  - files under {} bytes are not tracked individually, but directories always are, \
         so a tree of small files would still show its directories",
        opts.track_file_min_bytes
    );
    tracing::warn!(
        "  to see it from duTime's own point of view, run as the service user: \
         sudo -u dutime dutime scan {} --dry-run",
        root.display()
    );
}

/// What to do about an unreadable path, which depends on where it lives.
///
/// The generic advice — "grant CAP_DAC_READ_SEARCH, the system unit already
/// does" — is correct on a local filesystem and **useless on a network one**.
/// On NFS with `sec=sys` the client sends a numeric uid and gid and the
/// server decides; it cannot see a client capability, so granting one changes
/// nothing. Worse, `root_squash` is the default almost everywhere, so running
/// as root maps to `nobody` and reads *less* than an ordinary user.
///
/// Sending someone to check a capability that was never going to help costs
/// them an afternoon, so the filesystem type picks the message.
/// The same advice, with the concrete uids when a path is available.
///
/// "make the uid match" is the correct fix and still leaves the reader two
/// lookups away from acting on it. Both numbers are obtainable — `stat` on a
/// directory works without permission to read it, which is precisely the
/// case here — so duTime names them and the fix becomes a single edit.
fn permission_advice_for(root_fstype: Option<&str>, root: Option<&std::path::Path>) -> String {
    let uids = root.and_then(uid_mismatch).unwrap_or_default();
    match root_fstype {
        Some(fs) if crate::scan::mounts::is_server_authorized(fs) => format!(
            "this root is on {fs}, where permissions are enforced by the SERVER against the \
             uid/gid duTime presents — CAP_DAC_READ_SEARCH does nothing here, and root_squash \
             means running as root reads less, not more. Fix it by making the uid match: run \
             duTime as the user that owns the files, or grant that uid access on the server \
             (for a mode-700 directory, no group or capability will do).{uids}"
        ),
        Some(fs) => format!(
            "this root is on {fs} (a local filesystem), so CAP_DAC_READ_SEARCH does grant \
             read and traverse on everything. Check it with: systemctl show dutime -p \
             AmbientCapabilities — an empty value means the --user unit, which has none."
        ),
        None => "could not determine this root's filesystem type, so cannot say whether \
             CAP_DAC_READ_SEARCH would help (it does on local filesystems, and does nothing \
             on NFS/SMB, where the server checks the uid)."
            .to_string(),
    }
}

#[cfg(test)]
mod advice_tests {
    use super::*;

    /// The NFS branch cannot be reached by a test that has no NFS server, so
    /// the message itself is pinned here instead. What it must never do is
    /// tell someone to grant a capability that cannot work.
    #[test]
    fn a_network_filesystem_is_not_sent_to_check_capabilities() {
        for fs in ["nfs", "nfs4", "cifs", "smb3", "ceph", "afs"] {
            let a = permission_advice_for(Some(fs), None);
            assert!(a.contains("SERVER"), "{fs}: {a}");
            assert!(a.contains("uid"), "{fs}: {a}");
            assert!(
                a.contains("CAP_DAC_READ_SEARCH does nothing"),
                "{fs} was not told the capability is useless: {a}"
            );
            assert!(a.contains("root_squash"), "{fs}: {a}");
        }
    }

    #[test]
    fn a_local_filesystem_is_told_the_capability_helps() {
        for fs in ["ext4", "btrfs", "xfs", "zfs", "vfat"] {
            let a = permission_advice_for(Some(fs), None);
            assert!(a.contains("does grant"), "{fs}: {a}");
            assert!(a.contains("AmbientCapabilities"), "{fs}: {a}");
            assert!(!a.contains("SERVER"), "{fs} got the network advice: {a}");
        }
    }

    /// Saying nothing confidently is better than saying the wrong thing.
    #[test]
    fn an_unknown_filesystem_hedges_rather_than_guesses() {
        let a = permission_advice_for(None, None);
        assert!(a.contains("could not determine"), "{a}");
        // It still has to mention both cases, or it is no help at all.
        assert!(a.contains("local") && a.contains("NFS"), "{a}");
    }
}

/// " duTime runs as uid N; this root is owned by uid M (mode 0700)." — or
/// nothing, when they already match or the root cannot be stat-ed.
///
/// Saying nothing when the uids agree matters: on a share where the uid is
/// already right, the failure is something else entirely, and a line about
/// uids would be a confident red herring.
fn uid_mismatch(root: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::symlink_metadata(root).ok()?;
    let ours = rustix::process::geteuid().as_raw();
    let theirs = m.uid();
    if ours == theirs {
        return None;
    }
    // A directory that grants "other" read *and* execute is readable by
    // everyone, so a differing owner is not what is blocking the scan.
    // Claiming otherwise would point at a uid that was never the problem.
    if m.mode() & 0o005 == 0o005 {
        return None;
    }
    Some(format!(
        " Here that means: duTime runs as uid {ours}, this root is owned by uid {theirs} \
         with mode {:04o}{}. Set `User=` in `systemctl edit dutime` to the account with \
         uid {theirs}.",
        m.mode() & 0o7777,
        if m.mode() & 0o077 == 0 {
            ", which grants group and other nothing, so only that uid can read it"
        } else {
            ""
        }
    ))
}

#[cfg(test)]
mod uid_tests {
    use super::*;

    /// A share owned by somebody else and closed to others must name both
    /// numbers: "make the uid match" leaves the reader two lookups from
    /// acting on it.
    #[test]
    fn a_foreign_owner_is_named_by_number() {
        // /root is uid 0, mode 0700, and the test process is not root.
        if rustix::process::geteuid().is_root() {
            return;
        }
        let a = permission_advice_for(Some("nfs4"), Some(std::path::Path::new("/root")));
        assert!(a.contains("owned by uid 0"), "{a}");
        assert!(a.contains("systemctl edit dutime"), "{a}");
        assert!(a.contains("SERVER"), "{a}");
    }

    /// A world-readable directory is not blocked by its owner, so pointing at
    /// the uid would send someone to change the one thing that is fine.
    #[test]
    fn a_world_readable_directory_is_not_blamed_on_the_uid() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        // /proc is uid 0 but mode 0555.
        let a = permission_advice_for(Some("nfs4"), Some(std::path::Path::new("/proc")));
        assert!(
            !a.contains("duTime runs as uid"),
            "blamed the uid for a directory everyone can read: {a}"
        );
    }

    /// When the uids already agree the failure is something else, and a line
    /// about uids would be a confident red herring.
    #[test]
    fn a_matching_owner_says_nothing_about_uids() {
        let dir = tempfile::Builder::new().prefix("dutime-uid-").tempdir().unwrap();
        let a = permission_advice_for(Some("nfs4"), Some(dir.path()));
        assert!(!a.contains("duTime runs as uid"), "invented a uid mismatch: {a}");
        // The rest of the network advice still applies.
        assert!(a.contains("SERVER"), "{a}");
    }

    /// A mode that grants the group nothing is worth calling out, since the
    /// obvious fix — add duTime to the owning group — cannot work.
    #[test]
    fn a_mode_700_directory_says_the_group_will_not_help() {
        if rustix::process::geteuid().is_root() {
            return;
        }
        let a = permission_advice_for(Some("nfs4"), Some(std::path::Path::new("/root")));
        if a.contains("duTime runs as uid") {
            assert!(a.contains("grants group and other nothing"), "{a}");
        }
    }
}
