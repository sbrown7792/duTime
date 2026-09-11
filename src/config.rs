//! Configuration.
//!
//! TOML, with CLI flags and `DUTIME_*` environment variables overriding the
//! file. Defaults are chosen so that `dutime serve` with no configuration at
//! all does something sensible: track `$HOME`, hourly, on localhost.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: SocketAddr,
    pub db: PathBuf,
    /// Walker threads. Half the cores capped at 4 — this is a background
    /// service and must never be why an interactive process stutters.
    pub threads: usize,
    /// Scan every root once at startup rather than waiting a full interval.
    pub scan_on_start: bool,
    pub checkpoint_every_scans: i64,
    pub checkpoint_min_bytes: i64,
    /// Log a line per HTTP request: client, method, path, status, duration.
    ///
    /// Off by default because a browser sitting on the dashboard generates a
    /// steady trickle, and a journal that scrolls is a journal nobody reads.
    /// Turn it on to answer the question that matters when the page will not
    /// load: do requests reach us at all?
    pub access_log: bool,
    #[serde(rename = "root")]
    pub roots: Vec<RootConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RootConfig {
    pub path: PathBuf,
    /// Seconds between scans.
    ///
    /// The default is hourly, not five-minutely, on purpose. Every timing we
    /// have is from a warm cache; the first walk after a reboot has to fault
    /// in ~1 GB of dentries and inodes and will be far slower. Opt into five
    /// minutes once you have measured a cold scan on the machine in question.
    pub interval_s: u64,
    pub one_filesystem: bool,
    /// Files at least this large become tracked entities in their own right.
    pub track_file_min_bytes: i64,
    /// Gitignore-syntax patterns, matched relative to this root. A bare
    /// pattern excludes; `!` re-includes.
    pub exclude: Vec<String>,
    /// Absolute paths never to descend into.
    pub exclude_paths: Vec<PathBuf>,
}

impl Default for RootConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("."),
            interval_s: 3600,
            one_filesystem: true,
            track_file_min_bytes: 1 << 20,
            exclude: default_excludes(),
            exclude_paths: default_exclude_paths(),
        }
    }
}

/// Relative patterns worth skipping under any root.
///
/// These are matched *relative to the scan root*, so they must never be
/// absolute system paths. Writing `/snap/` here would mean `<root>/snap` —
/// which, scanning `$HOME` on Ubuntu, silently drops 71,516 files and 19 GB of
/// real user data. Absolute paths go in [`default_exclude_paths`].
pub fn default_excludes() -> Vec<String> {
    ["**/.cache/thumbnails/", "**/*.sock"].iter().map(|s| s.to_string()).collect()
}

/// Absolute paths not worth tracking on a typical Linux host.
///
/// Most of these are already skipped by the filesystem-type denylist, since
/// they are separate virtual mounts — but `/tmp` and `/var/tmp` are ordinary
/// directories on many installs, and listing the rest costs nothing and makes
/// the intent legible.
///
/// `/snap` is here not because of its device but because its contents are a
/// decompressed view of squashfs images already counted under
/// `/var/lib/snapd/snaps` — counting both reports the same 19 GB twice.
pub fn default_exclude_paths() -> Vec<PathBuf> {
    [
        "/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/snap",
        "/var/lib/docker/overlay2", "/var/lib/snapd/cache",
    ]
    .iter()
    .map(PathBuf::from)
    .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8471".parse().unwrap(),
            db: crate::cli::default_db_path(),
            threads: crate::scan::walker::default_threads(),
            scan_on_start: true,
            checkpoint_every_scans: 288,
            checkpoint_min_bytes: 1 << 20,
            access_log: false,
            roots: Vec::new(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&raw).with_context(|| format!("parsing config {}", p.display()))?
            }
            None => Config::default(),
        };

        if let Ok(v) = std::env::var("DUTIME_LISTEN") {
            cfg.listen = v.parse().context("DUTIME_LISTEN is not a host:port")?;
        }
        if let Ok(v) = std::env::var("DUTIME_DB") {
            cfg.db = PathBuf::from(v);
        }
        if let Ok(v) = std::env::var("DUTIME_ACCESS_LOG") {
            cfg.access_log = !matches!(v.as_str(), "" | "0" | "false" | "no");
        }

        if cfg.roots.is_empty() {
            // Nothing configured: track the user's home directory, which is
            // the one path we can be sure is both interesting and readable.
            if let Ok(home) = std::env::var("HOME") {
                cfg.roots.push(RootConfig { path: PathBuf::from(home), ..Default::default() });
            }
        }
        for r in &mut cfg.roots {
            if r.exclude.is_empty() {
                r.exclude = default_excludes();
            }
            if r.exclude_paths.is_empty() {
                r.exclude_paths = default_exclude_paths();
            }
        }
        Ok(cfg)
    }

    /// A commented starter config.
    pub fn sample() -> String {
        r#"# duTime configuration

# Bind address. "127.0.0.1" is reachable only from this machine; use
# "0.0.0.0" to serve the network.
#
# IMPORTANT, on a systemd *system* install: the shipped unit also carries
# "IPAddressAllow=localhost", which drops non-loopback packets no matter what
# you bind to here. Changing this line alone gives you a socket that is
# listening and unreachable at the same time -- a browser tab that spins
# forever with nothing in the log. Use `dutime install --system --listen
# 0.0.0.0:8471`, which writes both, or edit IPAddressAllow= in the unit to
# match. `dutime doctor` checks the two agree.
listen = "127.0.0.1:8471"
# db = "/var/lib/dutime/dutime.db"

# Log a line per HTTP request. Off by default; the first thing to turn on when
# the page will not load, since it separates "requests never arrive" from
# "requests arrive and something is slow".
# access_log = false

# Walker threads. Half the cores, capped at 4, by default.
# threads = 4

# Write inclusive keyframes every N scans. These bound how far an "as of T"
# query has to replay, so lower means faster history at the cost of more rows.
checkpoint_every_scans = 288
checkpoint_min_bytes = 1048576

[[root]]
path = "/home"
# Hourly by default. All published timings are warm-cache; measure a scan
# after a reboot before dropping this to 300.
interval_s = 3600
one_filesystem = true
# Files at least this big get tracked individually. On a typical machine 1 MiB
# covers ~95% of all bytes with ~3% of the file count.
track_file_min_bytes = 1048576
# Relative to this root -- "/snap/" here would mean "/home/snap", not the
# system /snap. Absolute paths belong in exclude_paths.
exclude = ["**/.cache/thumbnails/", "**/node_modules/.cache/", "**/*.sock"]
exclude_paths = ["/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/snap"]

# [[root]]
# path = "/var"
# interval_s = 3600
# exclude = ["/var/lib/docker/overlay2/**", "/var/lib/snapd/cache/**"]
"#
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sample we print must be a config we can load.
    ///
    /// `dutime config` and `dutime install` both emit this, so a typo here
    /// would hand the user a file that fails on the first start — after they
    /// have already enabled the service.
    #[test]
    fn sample_config_round_trips() {
        let cfg: Config = toml::from_str(&Config::sample()).expect("sample must parse");
        assert_eq!(cfg.listen.port(), 8471);
        assert_eq!(cfg.roots.len(), 1);
        assert_eq!(cfg.roots[0].path, PathBuf::from("/home"));
        assert_eq!(cfg.roots[0].track_file_min_bytes, 1 << 20);
        assert!(cfg.roots[0].one_filesystem);
    }

    /// `deny_unknown_fields` is on, so a typo'd key must be an error rather
    /// than silently ignored guidance the user thinks is in effect.
    #[test]
    fn rejects_unknown_keys() {
        let e = toml::from_str::<Config>("lissten = \"127.0.0.1:1\"").unwrap_err().to_string();
        assert!(e.contains("lissten") || e.contains("unknown"), "unhelpful error: {e}");
    }

    #[test]
    fn defaults_are_sane_without_a_file() {
        let c = Config::default();
        assert_eq!(c.listen.to_string(), "127.0.0.1:8471");
        assert!(c.threads >= 1 && c.threads <= 4);
        assert!(c.checkpoint_every_scans > 0);
    }

    /// The hourly default is deliberate; see the field docs.
    #[test]
    fn root_default_interval_is_hourly() {
        assert_eq!(RootConfig::default().interval_s, 3600);
    }
}
