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
    #[serde(default)]
    pub auth: AuthConfig,
    #[serde(rename = "root")]
    pub roots: Vec<RootConfig>,
}

/// Who may read the API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// A file holding one token. Preferred over `token`: a path in a config
    /// file is not a secret, whereas the secret itself in a config file gets
    /// copied into backups, pasted into issues and committed to git.
    pub token_file: Option<PathBuf>,
    /// The token inline. Works, but see above.
    pub token: Option<String>,
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
    /// Require the bearer token to see this root over the API.
    ///
    /// Per-root rather than global so a shared dashboard stays useful: the
    /// system disk can be visible to anyone on the LAN while a Nextcloud
    /// volume, whose *filenames* are the sensitive part, needs the token.
    /// An unauthenticated caller is not told this root exists.
    pub protected: bool,
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
            protected: false,
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
            auth: AuthConfig::default(),
            roots: Vec::new(),
        }
    }
}

/// Turn a TOML error into one that says what to do.
///
/// The table names in this file are field names, not labels — every tracked
/// directory is another `[[root]]`. That is not obvious, and guessing wrong
/// produces a service that refuses to start, so the error has to say it
/// rather than leaving the reader to infer it from a list of valid fields.
fn explain(path: &std::path::Path, e: toml::de::Error) -> anyhow::Error {
    let mut msg = format!("parsing config {}: {e}", path.display());
    if e.message().contains("unknown field") {
        msg.push_str(
            "\n\nnote: the names in [brackets] are fixed field names, not labels you choose.\n\
             To track another directory, add a second [[root]] block and change its path:\n\
             \n\
             \x20   [[root]]\n\
             \x20   path = \"/srv\"\n\
             \x20   interval_s = 3600\n\
             \n\
             \x20   [[root]]\n\
             \x20   path = \"/mnt/media\"\n\
             \x20   interval_s = 3600\n\
             \n\
             Each root is scanned and stored independently, and the web UI gets a\n\
             picker to switch between them. Check a file before restarting with\n\
             `dutime config --check <file>`.",
        );
    }
    anyhow::anyhow!(msg)
}

impl Config {
    pub fn load(path: Option<&std::path::Path>) -> Result<Self> {
        let mut cfg = match path {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .with_context(|| format!("reading config {}", p.display()))?;
                toml::from_str(&raw).map_err(|e| explain(p, e))?
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
        // `r##` rather than `r#`: the sample mentions "#recycle/", and a
        // `"#` inside would close a `r#"` string.
        r##"# duTime configuration

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

# A bearer token, required to view any root marked `protected = true` below.
# Generate one with:  sudo dutime token --write /etc/dutime/token
# Sign in from the web UI with the lock button in the header.
#
# Unprotected roots stay visible without it, so a shared dashboard can show
# the system disk to anyone on the LAN while a volume whose *filenames* are
# the sensitive part stays shut. An anonymous visitor is not told that a
# protected root exists.
# [auth]
# token_file = "/etc/dutime/token"

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
#
# Gitignore syntax: a bare pattern excludes, "!" re-includes. A leading "#"
# is taken literally, so a Synology share's "#recycle/" works as written.
exclude = ["**/.cache/thumbnails/", "**/node_modules/.cache/", "**/*.sock"]
exclude_paths = ["/proc", "/sys", "/dev", "/run", "/tmp", "/var/tmp", "/snap"]

# Track as many directories as you like: each one is another [[root]] block.
# The name in brackets is a fixed field name, not a label -- [[media]] or
# [[drive2]] will be rejected. Roots are scanned and stored independently, and
# the web UI gets a picker to switch between them.
#
# [[root]]
# path = "/mnt/nextcloud"
# interval_s = 3600
# Needs the token above. Sizes, growth and filenames are all hidden from
# anyone who has not signed in -- including the fact that this root exists.
# protected = true
# A big, slow, rarely-changing drive wants a coarser threshold: tracking every
# 1 MiB file on a media volume is a lot of rows about things that never move.
# track_file_min_bytes = 104857600
"##
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

    /// Several roots is the ordinary case, not a special one.
    #[test]
    fn multiple_roots_parse() {
        let c: Config = toml::from_str(
            r#"
listen = "127.0.0.1:8471"
[[root]]
path = "/"
interval_s = 3600
[[root]]
path = "/mnt/media"
interval_s = 300
"#,
        )
        .unwrap();
        assert_eq!(c.roots.len(), 2);
        assert_eq!(c.roots[1].path, PathBuf::from("/mnt/media"));
        assert_eq!(c.roots[1].interval_s, 300);
    }

    /// Renaming the table is the natural mistake — it looks like a label —
    /// and the error has to teach the fix, not just reject the file.
    #[test]
    fn a_renamed_root_table_explains_itself() {
        let err = Config::load(Some(&write_tmp(
            "renamed.toml",
            "[[root]]\npath = \"/\"\n\n[[media]]\npath = \"/mnt/media\"\n",
        )))
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field `media`"), "{err}");
        assert!(err.contains("[[root]]"), "no fix offered: {err}");
        assert!(err.contains("not labels you choose"), "{err}");
    }

    /// A valid file must not acquire the hint.
    #[test]
    fn a_good_config_is_not_lectured() {
        let p = write_tmp("fine.toml", "[[root]]\npath = \"/\"\n");
        assert!(Config::load(Some(&p)).is_ok());
    }

    fn write_tmp(name: &str, body: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("dutime-cfgtest-{name}"));
        std::fs::write(&p, body).unwrap();
        p
    }
}
