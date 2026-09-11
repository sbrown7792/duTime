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

#[derive(Parser)]
#[command(name = "dutime", version, about = "Track disk usage over time")]
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
        /// RFC3339, a relative offset like -24h or -7d, or scan:<id>.
        #[arg(long, default_value = "now")]
        at: String,
    },

    /// What grew (or shrank) over a window.
    Top {
        /// Window length, e.g. 24h, 7d.
        #[arg(long, default_value = "24h")]
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

    /// Check the database for internal inconsistency.
    Doctor,
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
        Cmd::Doctor => cmd_doctor(&db_path),
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
    for m in &r.stats.skipped_mounts {
        println!("{:<18} {}", "skipped mount", m.display());
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
            if resolved != t {
                // Never let the caller believe we have a sample we don't.
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

fn cmd_doctor(db_path: &Path) -> Result<()> {
    let store = Store::open(db_path)?;
    let mut problems = 0;

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

fn fmt_time(epoch: i64) -> String {
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
