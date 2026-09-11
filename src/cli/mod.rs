//! Command-line interface.
//!
//! The CLI is not an afterthought to the web UI: it is what people actually
//! script against, and it delivers the product's value before any chart
//! exists. `dutime top --since 7d` answers the question duTime was built for.

pub mod timespec;

use crate::model::{Metric, Mode};
use crate::scan::walker::{ScanOptions, scan};
use crate::store::commit::{CommitOptions, commit_scan};
use crate::store::{Store, query};
use anyhow::{Context, Result};
use bytesize::ByteSize;
use clap::{Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};

/// Version, commit and build date.
///
/// duTime is deployed by copying a binary to a server, so "is the thing
/// running there the thing I just built?" has to be answerable. A bare
/// semver cannot answer it: it is identical across every build between
/// releases, which is exactly the window in which the question gets asked.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("DUTIME_BUILD"), ")");

/// When this binary was linked, read from the binary itself at runtime.
///
/// The compile-time stamp above can go stale if the build script does not
/// rerun. This cannot: it is the mtime of the file currently executing. When
/// the question is "did my copy actually land", this is the answer.
pub fn build_mtime() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let t = std::fs::metadata(&exe).ok()?.modified().ok()?;
    let secs = t.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(crate::cli::fmt_time(secs))
}

#[derive(Parser)]
#[command(name = "dutime", version = VERSION, about = "Track disk usage over time")]
pub struct Cli {
    /// Database file. Defaults to $XDG_DATA_HOME/dutime/dutime.db, or
    /// /var/lib/dutime/dutime.db when running as a system service.
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Print raw byte counts instead of human-readable sizes.
    #[arg(long, global = true)]
    pub bytes: bool,

    /// Report allocated size (st_blocks) instead of apparent size (st_size).
    #[arg(long, global = true)]
    pub allocated: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Walk a directory once and record a snapshot.
    Scan {
        path: PathBuf,
        /// Track files at least this large as entities in their own right.
        #[arg(long, default_value = "1MiB", value_parser = parse_size)]
        min_file: i64,
        #[arg(long)]
        threads: Option<usize>,
        /// Gitignore-syntax pattern to skip. Repeatable.
        #[arg(long)]
        exclude: Vec<String>,
        /// Walk into other filesystems too.
        #[arg(long)]
        cross_filesystem: bool,
        /// Walk and report, but write nothing.
        #[arg(long)]
        dry_run: bool,
    },

    /// Size of a path, optionally as it was at some past moment.
    Du {
        path: PathBuf,
        /// RFC3339, a relative offset like -24h or 7d, or scan:<id>.
        #[arg(long, default_value = "now", allow_hyphen_values = true)]
        at: String,
    },

    /// What grew (or shrank) over a window.
    Top {
        /// Window length, e.g. 24h, 7d.
        #[arg(long, default_value = "24h", allow_hyphen_values = true)]
        since: String,
        /// Restrict to a subtree.
        #[arg(long)]
        under: Option<PathBuf>,
        /// exclusive names the directory whose own files grew; inclusive rolls
        /// growth up the ancestor chain.
        #[arg(long, value_enum, default_value_t = ModeArg::Exclusive)]
        mode: ModeArg,
        #[arg(long, default_value_t = 20)]
        limit: i64,
        /// Show the biggest shrinkers instead.
        #[arg(long)]
        losers: bool,
        /// In inclusive mode, list every ancestor rather than only the
        /// deepest directory that explains the growth.
        #[arg(long)]
        no_collapse: bool,
    },

    /// List recorded scans.
    Scans {
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },

    /// Run the service: scheduled scans plus the web UI.
    Serve {
        /// Config file. Without one, duTime tracks $HOME hourly on
        /// 127.0.0.1:8471.
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// Override the listen address.
        #[arg(long, short)]
        listen: Option<std::net::SocketAddr>,
        /// Track this path instead of whatever the config says. Repeatable.
        #[arg(long)]
        root: Vec<PathBuf>,
        /// Seconds between scans, when --root is given.
        #[arg(long, default_value = "3600", value_parser = parse_duration_arg)]
        interval: u64,
        /// Do not scan immediately on startup.
        #[arg(long)]
        no_initial_scan: bool,
    },

    /// Print a commented starter configuration.
    Config {
        /// Validate this file instead, and print what it resolves to.
        #[arg(long, value_name = "FILE")]
        check: Option<PathBuf>,
    },

    /// Write a systemd unit and a starter config.
    Install {
        /// Install for the current user only: no root, no capabilities, and
        /// it can only see what you can already read.
        #[arg(long, conflicts_with = "system")]
        user: bool,
        /// Install system-wide. Runs as a dedicated unprivileged `dutime`
        /// user holding only CAP_DAC_READ_SEARCH -- never as root.
        #[arg(long)]
        system: bool,
        /// Paths to track. Defaults to $HOME for --user, / for --system.
        #[arg(long)]
        root: Vec<PathBuf>,
        /// Print what would be written and exit.
        #[arg(long)]
        dry_run: bool,
        /// Bind address. Set it here rather than editing the config
        /// afterwards: this is what keeps the unit's IPAddressAllow= in step.
        /// A non-loopback address set in only one of the two places produces
        /// a service that listens and is unreachable at the same time.
        #[arg(long)]
        listen: Option<std::net::SocketAddr>,
    },

    /// Generate a bearer token for the web UI.
    Token {
        /// Write it to this file (mode 0600) instead of printing it.
        #[arg(long, value_name = "FILE")]
        write: Option<PathBuf>,
        /// Overwrite an existing file. Without this, an existing token is
        /// left alone — rotating it silently would lock out every browser
        /// and script already using it.
        #[arg(long)]
        force: bool,
    },

    /// Check the database for internal inconsistency.
    Doctor {
        /// Config file, so the checks can see the configured listen address.
        #[arg(long, short)]
        config: Option<PathBuf>,
        /// Skip the reachability checks (they can take a few seconds when the
        /// answer is bad, which is the interesting case).
        #[arg(long)]
        no_network: bool,
    },
}

fn parse_duration_arg(s: &str) -> Result<u64, String> {
    timespec::parse_duration(s).map(|v| v as u64).map_err(|e| e.to_string())
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    Exclusive,
    Inclusive,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Exclusive => Mode::Exclusive,
            ModeArg::Inclusive => Mode::Inclusive,
        }
    }
}

fn parse_size(s: &str) -> Result<i64, String> {
    s.parse::<ByteSize>().map(|b| b.as_u64() as i64).map_err(|e| e.to_string())
}

/// Where the database lives by default.
///
/// Running under a systemd unit with `StateDirectory=` sets `$STATE_DIRECTORY`,
/// which is the correct location for a system service and avoids hardcoding
/// /var/lib. Otherwise fall back to the XDG data directory for the user unit.
pub fn default_db_path() -> PathBuf {
    if let Ok(d) = std::env::var("STATE_DIRECTORY") {
        return PathBuf::from(d).join("dutime.db");
    }
    if let Ok(d) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(d).join("dutime/dutime.db");
    }
    if let Ok(h) = std::env::var("HOME") {
        return PathBuf::from(h).join(".local/share/dutime/dutime.db");
    }
    PathBuf::from("dutime.db")
}

pub struct Fmt {
    pub raw: bool,
    pub metric: Metric,
}

impl Fmt {
    pub fn size(&self, n: i64) -> String {
        if self.raw {
            n.to_string()
        } else if n < 0 {
            format!("-{}", ByteSize(n.unsigned_abs()))
        } else {
            ByteSize(n as u64).to_string()
        }
    }

    pub fn signed(&self, n: i64) -> String {
        if n > 0 { format!("+{}", self.size(n)) } else { self.size(n) }
    }

    pub fn pick(&self, bytes: i64, blocks: i64) -> i64 {
        match self.metric {
            Metric::Apparent => bytes,
            Metric::Allocated => blocks,
        }
    }
}

pub fn run(cli: Cli) -> Result<()> {
    let db_path = cli.db.clone().unwrap_or_else(default_db_path);
    let fmt = Fmt {
        raw: cli.bytes,
        metric: if cli.allocated { Metric::Allocated } else { Metric::Apparent },
    };

    match cli.cmd {
        Cmd::Scan { path, min_file, threads, exclude, cross_filesystem, dry_run } => {
            cmd_scan(&db_path, &fmt, path, min_file, threads, exclude, cross_filesystem, dry_run)
        }
        Cmd::Du { path, at } => cmd_du(&db_path, &fmt, &path, &at),
        Cmd::Top { since, under, mode, limit, losers, no_collapse } => cmd_top(
            &db_path,
            &fmt,
            &since,
            under.as_deref(),
            mode.into(),
            limit,
            losers,
            !no_collapse,
        ),
        Cmd::Scans { limit } => cmd_scans(&db_path, &fmt, limit),
        Cmd::Config { check } => match check {
            None => {
                print!("{}", crate::config::Config::sample());
                Ok(())
            }
            Some(p) => cmd_config_check(&p),
        },
        Cmd::Install { user, system, root, dry_run, listen } => {
            cmd_install(user, system, root, dry_run, listen)
        }
        Cmd::Serve { config, listen, root, interval, no_initial_scan } => {
            let mut cfg = crate::config::Config::load(config.as_deref())?;
            if let Some(db) = cli.db {
                cfg.db = db;
            }
            if let Some(l) = listen {
                cfg.listen = l;
            }
            if !root.is_empty() {
                cfg.roots = root
                    .into_iter()
                    .map(|path| crate::config::RootConfig {
                        path,
                        interval_s: interval,
                        ..Default::default()
                    })
                    .collect();
            }
            if no_initial_scan {
                cfg.scan_on_start = false;
            }
            if cfg.roots.is_empty() {
                anyhow::bail!(
                    "no roots to track — pass --root <path>, or set $HOME, or write a config \
                     (see `dutime config`)"
                );
            }
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?
                .block_on(crate::daemon::serve(cfg))
        }
        Cmd::Token { write, force } => cmd_token(write.as_deref(), force),
        Cmd::Doctor { config, no_network } => {
            // A config that names a database and a doctor that checks a
            // different one is worse than no check: it reports health for a
            // file the service never opens. An explicit --db still wins.
            let cfg = crate::config::Config::load(config.as_deref())?;
            let db = cli.db.clone().unwrap_or_else(|| cfg.db.clone());
            cmd_doctor(&db, &cfg, !no_network)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn cmd_scan(
    db_path: &Path,
    fmt: &Fmt,
    path: PathBuf,
    min_file: i64,
    threads: Option<usize>,
    exclude: Vec<String>,
    cross_filesystem: bool,
    dry_run: bool,
) -> Result<()> {
    let mut opts = ScanOptions::new(&path);
    opts.track_file_min_bytes = min_file;
    opts.exclude = exclude;
    opts.one_filesystem = !cross_filesystem;
    if let Some(t) = threads {
        opts.threads = t;
    }

    let started_at = now();
    let t0 = std::time::Instant::now();
    let r = scan(&opts)?;
    let walk_ms = t0.elapsed().as_millis() as i64;
    let roll = r.tree.rollup();
    let canon = path.canonicalize()?;

    println!("{:<18} {}", "path", canon.display());
    println!("{:<18} {}", "apparent", fmt.size(roll.bytes[0]));
    println!("{:<18} {}", "allocated", fmt.size(roll.blocks[0]));
    println!("{:<18} {}", "directories", r.stats.n_dirs);
    println!("{:<18} {}", "files", r.stats.n_files);
    println!("{:<18} {}", "tracked entities", r.tree.len());
    println!("{:<18} {}", "hardlinks deduped", r.stats.n_hardlinks_deduped);
    if r.stats.n_errors > 0 {
        println!("{:<18} {} (unreadable paths were skipped)", "errors", r.stats.n_errors);
    }
    // Separate filesystems first and unabridged: on a scan of / these are a
    // handful of real volumes, while the denied-fstype skips are 89 snap
    // images nobody wants listed.
    for m in &r.stats.other_filesystems {
        println!("{:<18} {} (its bytes are NOT in this total)", "other filesystem", m.display());
    }
    if !r.stats.skipped_mounts.is_empty() {
        println!(
            "{:<18} {} virtual or duplicate mount(s)",
            "skipped",
            r.stats.skipped_mounts.len()
        );
    }
    println!("{:<18} {:.3}s ({} threads)", "walk", walk_ms as f64 / 1000.0, opts.threads);

    if dry_run {
        println!("{:<18} nothing written (--dry-run)", "stored");
        return Ok(());
    }

    let mut store = Store::open(db_path)?;
    let root_id = store.ensure_root(&canon)?;
    let t1 = std::time::Instant::now();
    let st = commit_scan(
        &mut store,
        root_id,
        &canon,
        &r.tree,
        &roll,
        &r.stats,
        started_at,
        walk_ms,
        &CommitOptions::default(),
    )?;
    println!(
        "{:<18} scan {} — {} event(s), {} new, {} gone, {} keyframe(s) in {:.3}s",
        "stored",
        st.scan_id,
        st.n_events,
        st.n_born,
        st.n_gone + st.n_subtree_gone,
        st.n_checkpoints,
        t1.elapsed().as_secs_f64()
    );
    Ok(())
}

fn cmd_du(db_path: &Path, fmt: &Fmt, path: &Path, at: &str) -> Result<()> {
    let store = Store::open(db_path)?;
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let (root_id, path_id) = locate(&store, &canon)?;

    let scan_id = match timespec::parse(at, now())? {
        timespec::Target::Scan(id) => id,
        timespec::Target::At(t) => {
            let (id, resolved) = query::resolve_scan(&store, root_id, t)?
                .context("no scan recorded at or before that time")?;
            // Never let the caller believe we have a reading we never took --
            // but "now" always resolves backwards, so saying so there is noise.
            if resolved != t && at != "now" {
                eprintln!("note: nearest scan is {}", fmt_time(resolved));
            }
            id
        }
    };

    let (b, k) = query::incl_at(&store, path_id, scan_id)?;
    println!("{}\t{}", fmt.size(fmt.pick(b, k)), canon.display());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_top(
    db_path: &Path,
    fmt: &Fmt,
    since: &str,
    under: Option<&Path>,
    mode: Mode,
    limit: i64,
    losers: bool,
    collapse: bool,
) -> Result<()> {
    let store = Store::open(db_path)?;
    let roots = store.roots()?;
    let (root_id, _) = match under {
        Some(p) => {
            let canon = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
            let (rid, _) = locate(&store, &canon)?;
            (rid, canon)
        }
        None => roots.first().cloned().context("no roots recorded yet — run `dutime scan` first")?,
    };

    let t_now = now();
    let dur = timespec::parse_duration(since)?;
    let (s2, _) = query::resolve_scan(&store, root_id, t_now)?
        .context("no scans recorded yet — run `dutime scan` first")?;

    // If the requested window reaches back past the first scan, clamp it to
    // that scan rather than to zero.
    //
    // This matters more than it looks. The baseline scan records every entity
    // as newly born, so a window that includes it reports the entire disk as
    // "growth" — the first report a new user ever runs would claim their whole
    // home directory appeared in the last hour. Starting from the baseline
    // instead measures only what has actually changed since tracking began.
    let (s1, clamped) = match query::resolve_scan(&store, root_id, t_now - dur)? {
        Some((id, _)) => (id, None),
        None => match store.first_scan(root_id)? {
            Some((id, at)) => (id, Some(at)),
            None => (0, None),
        },
    };

    if let Some(at) = clamped {
        eprintln!(
            "note: tracking began at {}, so this covers less than {since}",
            fmt_time(at)
        );
    }

    if s1 == s2 {
        println!("only one scan in the last {since}; nothing to compare against yet");
        return Ok(());
    }

    let which = if losers { query::Extreme::Losers } else { query::Extreme::Gainers };
    let collapse = collapse && mode == Mode::Inclusive;
    // Collapsing discards rows, so ask for more candidates than we intend to
    // show or a deep chain would leave the list short.
    let fetch = if collapse { limit * 8 } else { limit };
    let mut g = match mode {
        Mode::Exclusive => query::gainers_exclusive(&store, root_id, s1, s2, fetch, which)?,
        Mode::Inclusive => query::gainers_inclusive(&store, root_id, s1, s2, fetch, which)?,
    };
    if collapse {
        g = query::collapse_ancestors(&g, 0.9);
    }
    g.truncate(limit as usize);

    if g.is_empty() {
        println!("nothing changed in the last {since}");
        return Ok(());
    }

    let label = if losers { "shrank" } else { "grew" };
    println!("what {label} in the last {since} ({} mode)\n", match mode {
        Mode::Exclusive => "exclusive",
        Mode::Inclusive => "inclusive",
    });
    for e in &g {
        let p = query::full_path(&store, e.path_id)?;
        println!("{:>12}  {}", fmt.signed(fmt.pick(e.delta_bytes, e.delta_blocks)), p.display());
    }
    Ok(())
}

fn cmd_scans(db_path: &Path, fmt: &Fmt, limit: i64) -> Result<()> {
    let store = Store::open(db_path)?;
    let mut st = store.conn.prepare(
        "SELECT s.scan_id, s.started_at, s.duration_ms, s.n_events, s.incl_bytes,
                s.incl_blocks, s.n_dirs, s.n_files, r.path
         FROM scan s JOIN root r ON r.root_id = s.root_id
         ORDER BY s.scan_id DESC LIMIT ?1",
    )?;
    let rows = st.query_map([limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, Vec<u8>>(8)?,
        ))
    })?;
    println!(
        "{:>5}  {:<20} {:>8} {:>8} {:>12}  {}",
        "scan", "when", "events", "walk", "size", "root"
    );
    for row in rows {
        let (id, at, ms, ev, b, k, _d, _f, rp) = row?;
        use std::os::unix::ffi::OsStringExt;
        let rp = std::path::PathBuf::from(std::ffi::OsString::from_vec(rp));
        println!(
            "{id:>5}  {:<20} {ev:>8} {:>7.2}s {:>12}  {}",
            fmt_time(at),
            ms as f64 / 1000.0,
            fmt.size(fmt.pick(b, k)),
            rp.display()
        );
    }
    Ok(())
}

fn cmd_doctor(db_path: &Path, cfg: &crate::config::Config, network: bool) -> Result<()> {
    let mut problems = 0;

    // Reachability comes first and never depends on the database. Someone
    // running `doctor` because a page will not load should not be met with
    // "no such file" from a check they did not ask about.
    println!("{:<28} {VERSION}", "version");
    println!(
        "{:<28} {} (built {})",
        "binary",
        std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default(),
        build_mtime().unwrap_or_else(|| "unknown".into())
    );
    println!("{:<28} {}", "database", db_path.display());
    if network {
        problems += doctor_network(cfg)?;
    }
    println!();

    let store = Store::open(db_path)
        .with_context(|| format!("opening {} — has a scan ever run?", db_path.display()))?;

    let integrity: String =
        store.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    println!("{:<28} {}", "sqlite integrity_check", integrity);
    if integrity != "ok" {
        problems += 1;
    }

    for (root_id, path) in store.roots()? {
        let drifted = crate::store::commit::verify_current_size(&store, root_id)?;
        println!(
            "{:<28} {} ({})",
            "current_size consistency",
            if drifted.is_empty() { "ok".into() } else { format!("{} drifted", drifted.len()) },
            path.display()
        );
        if !drifted.is_empty() {
            problems += 1;
        }

        // Reconstruction must agree with what the scanner recorded at the time.
        if let Some(last) = store.last_scan(root_id)? {
            let root_pid: i64 = store.conn.query_row(
                "SELECT path_id FROM path WHERE root_id = ?1 AND parent_id IS NULL",
                [root_id],
                |r| r.get(0),
            )?;
            let recorded: i64 = store.conn.query_row(
                "SELECT incl_bytes FROM scan WHERE scan_id = ?1",
                [last],
                |r| r.get(0),
            )?;
            let (fast, _) = query::incl_at(&store, root_pid, last)?;
            let (naive, _) = query::incl_at_naive(&store, root_pid, last)?;
            let ok = fast == recorded && naive == recorded;
            println!(
                "{:<28} {} (recorded {recorded}, fast {fast}, replay {naive})",
                "reconstruction",
                if ok { "ok" } else { "MISMATCH" }
            );
            if !ok {
                problems += 1;
            }

            // The in-RAM snapshot is a third, independent implementation of
            // the same rollup. If it disagrees with the SQL paths, the web UI
            // would quietly show different numbers from the CLI.
            let t0 = std::time::Instant::now();
            let snap = crate::store::snapshot::Snapshot::load(&store, root_id, last)?;
            let load_ms = t0.elapsed().as_millis();
            let snap_total = snap.root().map(|r| snap.incl_bytes[r as usize]).unwrap_or(-1);
            let snap_ok = snap_total == recorded;
            println!(
                "{:<28} {} ({} entities, loaded in {load_ms} ms)",
                "in-memory snapshot",
                if snap_ok { "ok".to_string() } else { format!("MISMATCH {snap_total}") },
                snap.len()
            );
            if !snap_ok {
                problems += 1;
            }
        }
    }

    let db_bytes = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
    let events: i64 = store.conn.query_row("SELECT COUNT(*) FROM size_event", [], |r| r.get(0))?;
    let paths: i64 = store.conn.query_row("SELECT COUNT(*) FROM path", [], |r| r.get(0))?;
    let live: i64 = store
        .conn
        .query_row("SELECT COUNT(*) FROM path WHERE died_scan IS NULL", [], |r| r.get(0))?;
    println!("{:<28} {}", "database size", ByteSize(db_bytes));
    println!("{:<28} {events}", "size_event rows");
    println!("{:<28} {paths} ({live} live)", "path rows");

    if problems == 0 {
        println!("\nno problems found");
        Ok(())
    } else {
        anyhow::bail!("{problems} problem(s) found")
    }
}

/// Find the root and path_id for an absolute filesystem path.
fn locate(store: &Store, path: &Path) -> Result<(i64, i64)> {
    use std::os::unix::ffi::OsStrExt;
    let roots = store.roots()?;
    let (root_id, root_path) = roots
        .iter()
        .filter(|(_, rp)| path.starts_with(rp))
        .max_by_key(|(_, rp)| rp.components().count())
        .cloned()
        .with_context(|| {
            format!(
                "{} is not inside any tracked root. Tracked: {}",
                path.display(),
                if roots.is_empty() {
                    "(none — run `dutime scan` first)".to_string()
                } else {
                    roots.iter().map(|(_, p)| p.display().to_string()).collect::<Vec<_>>().join(", ")
                }
            )
        })?;

    let mut path_id: i64 = store.conn.query_row(
        "SELECT path_id FROM path WHERE root_id = ?1 AND parent_id IS NULL",
        [root_id],
        |r| r.get(0),
    )?;
    for comp in path.strip_prefix(&root_path)?.iter() {
        path_id = store
            .conn
            .query_row(
                "SELECT path_id FROM path
                 WHERE parent_id = ?1 AND name = ?2 AND died_scan IS NULL",
                rusqlite::params![path_id, comp.as_bytes()],
                |r| r.get(0),
            )
            .with_context(|| format!("{} is not tracked (below the size threshold, excluded, or never scanned)", path.display()))?;
    }
    Ok((root_id, path_id))
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn fmt_time(epoch: i64) -> String {
    // Deliberately dependency-free: a fixed civil-time rendering of a UTC
    // epoch. Storage and comparison are always epoch seconds; this is only for
    // display.
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Howard Hinnant's days-from-civil, inverted. Valid for all years we care
/// about and avoids pulling in a date library for one format string.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
pub(crate) fn civil_from_days_pub(z: i64) -> (i64, u32, u32) {
    civil_from_days(z)
}

const USER_UNIT: &str = include_str!("../../packaging/dutime-user.service");
const SYSTEM_UNIT: &str = include_str!("../../packaging/dutime.service");

/// Write a systemd unit and a starter config.
///
/// Deliberately never runs `systemctl` itself. Installing a background service
/// that will read your whole filesystem is something an administrator should
/// see coming, so this writes the files, prints the two commands, and stops.
fn cmd_install(
    user: bool,
    system: bool,
    roots: Vec<PathBuf>,
    dry_run: bool,
    listen: Option<std::net::SocketAddr>,
) -> Result<()> {
    // Default to whichever mode needs no privilege we do not already have.
    let user = if user || system { user } else { !rustix::process::geteuid().is_root() };

    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".into());
    let (unit_path, cfg_path, unit, default_root) = if user {
        (
            PathBuf::from(&home).join(".config/systemd/user/dutime.service"),
            PathBuf::from(&home).join(".config/dutime/config.toml"),
            USER_UNIT,
            PathBuf::from(&home),
        )
    } else {
        (
            PathBuf::from("/etc/systemd/system/dutime.service"),
            PathBuf::from("/etc/dutime/config.toml"),
            SYSTEM_UNIT,
            PathBuf::from("/"),
        )
    };

    let roots = if roots.is_empty() { vec![default_root] } else { roots };

    // Start from the commented sample, then replace its example root blocks
    // with the ones actually requested.
    let mut cfg = crate::config::Config::sample();
    if let Some(cut) = cfg.find("[[root]]") {
        cfg.truncate(cut);
    }
    if let Some(l) = listen {
        cfg = cfg.replace("listen = \"127.0.0.1:8471\"", &format!("listen = \"{l}\""));
    }
    let unit = apply_listen(unit, listen);
    let excludes = crate::config::default_excludes()
        .iter()
        .map(|e| format!("{e:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    for r in &roots {
        cfg.push_str(&format!(
            "[[root]]\npath = {:?}\ninterval_s = 3600\none_filesystem = true\n\
             track_file_min_bytes = 1048576\nexclude = [{}]\n\n",
            r.display().to_string(),
            excludes
        ));
    }

    println!("{:<12} {}", "mode", if user { "user (no privilege)" } else { "system (CAP_DAC_READ_SEARCH)" });
    println!("{:<12} {}", "unit", unit_path.display());
    println!("{:<12} {}", "config", cfg_path.display());
    for r in &roots {
        println!("{:<12} {}", "tracking", r.display());
    }

    if dry_run {
        println!("\n----- {} -----\n{unit}", unit_path.display());
        println!("----- {} -----\n{cfg}", cfg_path.display());
        println!("(nothing written: --dry-run)");
        return Ok(());
    }

    for p in [&unit_path, &cfg_path] {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
    }
    // Never clobber an existing config: it is the one file with local edits.
    if cfg_path.exists() {
        println!("\nkept existing config: {}", cfg_path.display());
    } else {
        std::fs::write(&cfg_path, &cfg)
            .with_context(|| format!("writing {}", cfg_path.display()))?;
    }
    std::fs::write(&unit_path, &unit)
        .with_context(|| format!("writing {}", unit_path.display()))?;

    let sc = if user { "systemctl --user" } else { "sudo systemctl" };
    if !user {
        println!("\nthe unit runs as a dedicated unprivileged user; create it if needed:");
        println!("  sudo useradd --system --no-create-home --shell /usr/sbin/nologin dutime");
    }
    println!("\nthen:");
    println!("  {sc} daemon-reload");
    println!("  {sc} enable --now dutime");
    println!("\nverify it is actually reachable, which is not the same as running:");
    println!("  dutime doctor --config {}", cfg_path.display());
    Ok(())
}

/// Keep the unit's IP filter in step with the address being bound.
///
/// The unit ships loopback-only on purpose — a disk inventory of the whole
/// filesystem should not become network-visible because someone ran an
/// install command. But when the operator does ask for a network address, the
/// filter has to move with it, or they get a service that starts, listens,
/// logs nothing wrong, and drops every packet. That failure is invisible from
/// every angle an operator normally looks from, so it must not be possible to
/// reach it by using the tool the documented way.
fn apply_listen(unit: &str, listen: Option<std::net::SocketAddr>) -> String {
    let Some(l) = listen else { return unit.to_string() };
    if l.ip().is_loopback() {
        return unit.to_string();
    }
    // Anything reachable enough to be worth binding is reachable from
    // somewhere; we cannot know the client subnet, so open it and say so
    // rather than guessing a range that silently excludes the operator.
    unit.replace(
        "IPAddressAllow=localhost\nIPAddressDeny=any",
        &format!(
            "# Relaxed by `dutime install --listen {l}`.\n\
             # The shipped default is loopback-only:\n\
             #   IPAddressAllow=localhost\n\
             #   IPAddressDeny=any\n\
             # Narrow this to your client subnet if you can, e.g.\n\
             #   IPAddressAllow=192.168.0.0/16\n\
             #   IPAddressDeny=any\n\
             IPAddressAllow=any"
        ),
    )
}

/// Every address of this host a client might plausibly type.
///
/// The probe uses the routing table's choice of source address, which on a
/// host with a VPN or several NICs is frequently not the one the operator
/// will use. Listing them all costs nothing and removes a guess.
fn host_addresses() -> Vec<(String, String)> {
    let Ok(out) = std::process::Command::new("ip")
        .args(["-o", "addr", "show", "scope", "global"])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let iface = f.get(1)?;
            let addr = f.get(3)?.split('/').next()?;
            // Bracket IPv6 so the printed URL is one you can paste.
            let addr =
                if addr.contains(':') { format!("[{addr}]") } else { addr.to_string() };
            Some((iface.to_string(), addr))
        })
        .collect()
}



/// Can a browser actually reach us?
///
/// This section exists because of a specific, humiliating failure mode that
/// duTime shipped with: the system unit sets `IPAddressAllow=localhost`, so
/// changing `listen` to `0.0.0.0` in the config yields a service that starts
/// cleanly, logs that it is listening, holds an open port that `ss` and
/// `netstat` both confirm — and silently drops every packet from the network.
/// The operator sees a spinning tab and a perfectly healthy `systemctl
/// status`. Nothing anywhere says why, because a dropped packet generates no
/// error for anyone to report.
fn doctor_network(cfg: &crate::config::Config) -> Result<usize> {
    use crate::diag;
    use std::time::Duration;

    let mut problems = 0;
    let listen = cfg.listen;

    println!("{:<28} {listen}", "listen address");
    println!("{:<28} {}", "access log", if cfg.access_log { "on" } else { "off (set access_log = true to log each request)" });

    // What systemd will let through, which is a different question from what
    // we bound to — and the two disagreeing is the whole point of this check.
    let filters = systemd_ip_filters();
    match &filters {
        Some((unit, allow, deny)) => {
            println!("{:<28} {unit}", "systemd unit");
            println!("{:<28} {}", "IPAddressAllow", if allow.is_empty() { "(unset)" } else { allow });
            println!("{:<28} {}", "IPAddressDeny", if deny.is_empty() { "(unset)" } else { deny });
        }
        None => {
            println!("{:<28} not found (not installed, or not running under systemd)", "systemd unit")
        }
    }

    // The mismatch, stated plainly.
    if let Some((unit, allow, deny)) = &filters {
        let filtered = !deny.is_empty();
        let loopback_only = allow.split_whitespace().all(|a| {
            a.starts_with("127.") || a.starts_with("::1") || a == "localhost"
        });
        if filtered && loopback_only && !listen.ip().is_loopback() {
            problems += 1;
            println!(
                "\nPROBLEM  listen is {listen} but {unit} allows only {allow}.\n\
                 \x20        The socket is open and every packet from the network is dropped.\n\
                 \x20        A browser shows this as a tab that spins and never errors.\n\
                 \x20 fix    sudo dutime install --system --listen {listen}\n\
                 \x20        sudo systemctl daemon-reload && sudo systemctl restart dutime\n\
                 \x20 or     add the client subnet yourself:\n\
                 \x20        sudo systemctl edit dutime   # [Service] IPAddressAllow=192.168.0.0/16"
            );
        }
    }

    // Then go and actually try it, which catches everything the parsing above
    // does not think of.
    match diag::external_target(listen) {
        None => {
            println!(
                "\n{:<28} n/a — bound to loopback, so only this machine can connect",
                "remote reachability"
            );
            println!(
                "{:<28} ssh -N -L {}:localhost:{} <this-host>   then open http://localhost:{}",
                "  to reach it remotely",
                listen.port(),
                listen.port(),
                listen.port()
            );
        }
        Some(target) => {
            print!("{:<28} {target} ... ", "remote reachability");
            use std::io::Write;
            let _ = std::io::stdout().flush();
            let r = diag::probe(target, Duration::from_secs(3));
            if r.ok() {
                // Say exactly what was and was not proved. This packet went
                // over loopback and never met the external interface, so it
                // cleared systemd's filter and told us nothing about ufw.
                println!("ok");
                println!(
                    "{:<28} this leaves from this host, so it clears systemd's filter but not a firewall",
                    "  note"
                );
            } else {
                problems += 1;
                println!("FAILED");
                println!("{:<28} {}", "  reason", r.advice());
            }
            for (iface, addr) in host_addresses() {
                println!("{:<28} http://{addr}:{}/   ({iface})", "  try", listen.port());
            }
        }
    }

    // A host firewall is the other half, and we cannot test it from here.
    if let Some(state) = ufw_state() {
        println!("{:<28} {state}", "ufw");
    }
    println!(
        "{:<28} check the client is on an allowed subnet, then `sudo ufw allow <port>/tcp`",
        "  if still unreachable"
    );
    println!(
        "{:<28} duTime speaks plain HTTP — https:// to this port hangs the same way",
        "  and check the scheme"
    );

    Ok(problems)
}

/// Our own unit's IP filtering, as systemd resolved it.
///
/// Asking `systemctl` rather than re-parsing the unit file: drop-ins,
/// `systemctl edit`, and `localhost` expanding to `127.0.0.0/8 ::1/128` all
/// mean the file on disk is not the policy in force.
fn systemd_ip_filters() -> Option<(String, String, String)> {
    for (unit, scope) in [("dutime.service", "--system"), ("dutime.service", "--user")] {
        let out = std::process::Command::new("systemctl")
            .args([scope, "show", unit, "-p", "IPAddressAllow", "-p", "IPAddressDeny", "-p", "LoadState"])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k))
                .unwrap_or("")
                .trim()
                .to_string()
        };
        if field("LoadState=") == "loaded" {
            let label = if scope == "--user" { format!("{unit} (user)") } else { unit.to_string() };
            return Some((label, field("IPAddressAllow="), field("IPAddressDeny=")));
        }
    }
    None
}

/// ufw's own summary, when it will tell us without root.
fn ufw_state() -> Option<String> {
    let out = std::process::Command::new("ufw").arg("status").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    if let Some(l) = text.lines().find(|l| l.starts_with("Status:")) {
        return Some(l.trim().to_string());
    }
    // Without root ufw refuses; that is still worth saying, because it means
    // "there is a ufw here and I could not see its rules".
    Some("installed; run `sudo ufw status` to see its rules".into())
}



/// Parse a config and print what it actually resolves to.
///
/// Editing `/etc/dutime/config.toml` and finding out whether it was valid by
/// restarting the service is a bad loop: the feedback is a failed unit and a
/// journal entry. This gives the answer directly, and shows the resolved
/// values rather than only reporting the absence of an error — a config that
/// parses can still track a directory you did not mean, and the defaults that
/// get filled in are not visible in the file.
fn cmd_config_check(path: &Path) -> Result<()> {
    let cfg = crate::config::Config::load(Some(path))?;
    let mut problems = 0usize;
    println!("{:<28} {}", "config", path.display());
    println!("{:<28} ok", "parse");
    println!("{:<28} {}", "listen", cfg.listen);
    println!("{:<28} {}", "database", cfg.db.display());
    println!("{:<28} {}", "walker threads", cfg.threads);
    println!("{:<28} {}", "access log", if cfg.access_log { "on" } else { "off" });

    let auth = crate::auth::Auth::from_config(&cfg.auth)?;
    let n_protected = cfg.roots.iter().filter(|r| r.protected).count();
    println!(
        "{:<28} {}",
        "auth token",
        match (&auth, cfg.auth.token_file.as_ref()) {
            (crate::auth::Auth::Open, _) => "none configured".to_string(),
            (_, Some(f)) => format!("loaded from {}", f.display()),
            _ => "configured inline".to_string(),
        }
    );
    println!("{:<28} {n_protected} of {}", "protected roots", cfg.roots.len());
    // The same two mistakes the daemon checks at startup, reported before a
    // restart rather than by one.
    if n_protected > 0 && auth.is_open() {
        problems += 1;
        println!(
            "\nPROBLEM  {n_protected} root(s) are marked `protected` but no token is \
             configured,\n\x20        so nothing is actually protected and the service will \
             refuse to start.\n\x20 fix    sudo dutime token --write /etc/dutime/token, then \
             set [auth] token_file"
        );
    } else if !auth.is_open() && n_protected == 0 {
        println!(
            "\nnote     a token is configured but no root is marked `protected`, so it is \
             never required"
        );
    }

    if cfg.roots.is_empty() {
        println!("\nno [[root]] blocks: nothing would be tracked");
        anyhow::bail!("config tracks nothing");
    }

    let mounts = crate::scan::mounts::MountTable::load();
    println!("\n{} root(s):", cfg.roots.len());
    for r in &cfg.roots {
        let exists = r.path.is_dir();
        println!(
            "\n  {}{}",
            r.path.display(),
            if exists { "" } else { "   [does not exist or is not a directory]" }
        );
        println!("    {:<22} {}", "interval", humantime::format_duration(
            std::time::Duration::from_secs(r.interval_s)
        ));
        println!("    {:<22} {}", "track files over", ByteSize(r.track_file_min_bytes as u64));
        // The filesystem type decides what an unreadable path means here, so
        // it belongs next to the root rather than only in a scan warning.
        if let Some(e) = mounts.as_ref().ok().and_then(|mt| mt.find_mount_for(&r.path)) {
            let server = crate::scan::mounts::is_server_authorized(&e.fstype);
            println!(
                "    {:<22} {}{}",
                "filesystem",
                e.fstype,
                if server {
                    "  — the SERVER checks permissions against duTime's uid;                      CAP_DAC_READ_SEARCH does not apply"
                } else {
                    ""
                }
            );
        }
        println!("    {:<22} {}", "one filesystem", r.one_filesystem);
        println!(
            "    {:<22} {}",
            "protected",
            if r.protected { "yes — token required" } else { "no — visible to anyone" }
        );
        println!("    {:<22} {}", "exclude", fmt_list(&r.exclude));
        // Show only the absolute excludes that bear on *this* root. The
        // defaults include /tmp and /proc, and printing them verbatim under a
        // root like /tmp/media reads as "this root is excluded" when the
        // walker will in fact ignore a prefix that contains its own root.
        let (applies, inert): (Vec<_>, Vec<_>) = r
            .exclude_paths
            .iter()
            .map(|p| p.display().to_string())
            .partition(|p| {
                let p = Path::new(p);
                p.starts_with(&r.path) && p != r.path
            });
        println!("    {:<22} {}", "exclude_paths", fmt_list(&applies));
        if !inert.is_empty() {
            println!("    {:<22} {} (outside this root)", "  not applicable", inert.len());
        }
    }

    // A root inside another root is scanned twice and reported twice. It is
    // legal, so this is a warning rather than a failure, but it is almost
    // never what someone means.
    for a in &cfg.roots {
        for b in &cfg.roots {
            if a.path != b.path && a.path.starts_with(&b.path) {
                println!(
                    "\nwarning: {} is inside {} — both are scanned, so its bytes are counted \
                     under each",
                    a.path.display(),
                    b.path.display()
                );
            }
        }
    }

    let missing: Vec<_> = cfg.roots.iter().filter(|r| !r.path.is_dir()).collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "{} root(s) do not exist — the service will start but they will never scan",
            missing.len()
        );
    }
    if problems > 0 {
        anyhow::bail!("{problems} problem(s) found");
    }
    Ok(())
}

fn fmt_list(v: &[String]) -> String {
    if v.is_empty() { "(none)".into() } else { v.join(", ") }
}



/// Generate a token, and say what to do with it.
///
/// Exists so that nobody has to invent their own. A hand-picked token is
/// short, memorable and guessable, and the temptation to reuse a password
/// here is strong — this is 256 bits from the kernel CSPRNG, which removes
/// the decision.
fn cmd_token(write: Option<&Path>, force: bool) -> Result<()> {
    let token = crate::auth::generate()?;

    let Some(path) = write else {
        println!("{token}");
        eprintln!();
        eprintln!("Save it, then point the config at it:");
        eprintln!("  sudo dutime token --write /etc/dutime/token");
        eprintln!("  # then, in the config:");
        eprintln!("  [auth]");
        eprintln!("  token_file = \"/etc/dutime/token\"");
        return Ok(());
    };

    if path.exists() && !force {
        anyhow::bail!(
            "{} already exists. Replacing a token signs out every browser and breaks \
             every script using it, so pass --force if that is what you want.",
            path.display()
        );
    }
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    // Created 0600 from the outset. Writing it world-readable and chmod-ing
    // afterwards leaves a window in which the secret is readable by anyone.
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("writing {}", path.display()))?;
        writeln!(f, "{token}")?;
    }

    println!("{:<14} {}", "wrote", path.display());
    println!("{:<14} 0600", "mode");
    println!("{:<14} {token}", "token");
    println!();
    println!("Add to the config, then restart:");
    println!("  [auth]");
    println!("  token_file = \"{}\"", path.display());
    println!();
    println!("Mark the roots you want gated with `protected = true`, then sign in at the");
    println!("web UI with the lock button in the header and paste the token above.");
    Ok(())
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn loopback_leaves_the_filter_alone() {
        let u = apply_listen(SYSTEM_UNIT, Some("127.0.0.1:8471".parse().unwrap()));
        let live = |k: &str| u.lines().any(|l| l.trim() == k);
        assert!(live("IPAddressAllow=localhost"));
        assert!(live("IPAddressDeny=any"));
        assert_eq!(u, apply_listen(SYSTEM_UNIT, None));
    }

    /// The whole point: binding the network must not leave a filter behind
    /// that drops the network.
    #[test]
    fn a_network_address_relaxes_the_filter() {
        let u = apply_listen(SYSTEM_UNIT, Some("0.0.0.0:8471".parse().unwrap()));
        // Checked per line: the replacement text *documents* the old
        // directives in comments, so a substring search cannot tell a live
        // setting from a commented one.
        let live = |k: &str| u.lines().any(|l| l.trim() == k);
        assert!(!live("IPAddressDeny=any"), "deny survived:\n{u}");
        assert!(live("IPAddressAllow=any"), "allow not applied:\n{u}");
        // and it must still be a unit, not shredded
        assert!(u.contains("ExecStart=/usr/bin/dutime serve"));
    }

    /// The string being patched has to exist, or the substitution is a no-op
    /// that reintroduces the bug in silence.
    #[test]
    fn the_shipped_unit_contains_what_we_patch() {
        assert!(
            SYSTEM_UNIT.contains("IPAddressAllow=localhost\nIPAddressDeny=any"),
            "the unit's IP filter lines moved; apply_listen is now a no-op"
        );
    }
}
