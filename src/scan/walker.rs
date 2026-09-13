//! The filesystem walk.
//!
//! Produces a [`Tree`] of exclusive ("own") sizes for one root. Correctness
//! here is measured against exactly one oracle: `incl_bytes(root)` must equal
//! `du -sxb <root>` byte for byte, and `incl_blocks(root)` must equal
//! `du -sx <root> * 1024`. Every number the UI ever shows inherits from this,
//! so the walk deliberately replicates `du`'s accounting semantics rather than
//! inventing its own.

use crate::model::tree::{NodeIdx, Tree};
use crate::model::{Entity, Kind, flags};
use crate::scan::mounts::MountTable;
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

/// How a single directory entry was classified by the walk.
#[derive(Debug, Clone)]
struct RawEntry {
    rel: Vec<OsString>,
    kind: Kind,
    bytes: i64,
    blocks: i64,
    ino: u64,
    dev: u64,
    nlink: u64,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub root: PathBuf,
    /// Files at or above this size become tracked entities in their own right.
    ///
    /// Default 1 MiB. Measured on this machine: 24,158 files of 874k (2.8%)
    /// are >= 1 MiB and they hold ~95% of all bytes — so the threshold buys a
    /// 36x reduction in tracked file entities for a 5% loss of resolution.
    pub track_file_min_bytes: i64,
    pub one_filesystem: bool,
    /// Gitignore-syntax patterns, matched *relative to the scan root*. A bare
    /// pattern excludes; a `!` prefix re-includes. Note this is gitignore
    /// semantics, not ripgrep `--glob` semantics where a bare pattern would
    /// whitelist.
    ///
    /// Because these are root-relative, a pattern like `/snap/` does **not**
    /// mean the system `/snap`: scanning `$HOME` it would match
    /// `$HOME/snap`, silently dropping 19 GB of real user data. Absolute
    /// system paths belong in [`ScanOptions::exclude_prefixes`].
    pub exclude: Vec<String>,
    /// Absolute paths never to descend into, matched as literal prefixes.
    ///
    /// This is the right home for `/proc`, `/tmp` and friends: an absolute
    /// path means the same thing regardless of which root is being scanned.
    pub exclude_prefixes: Vec<PathBuf>,
    pub threads: usize,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            track_file_min_bytes: 1 << 20,
            one_filesystem: true,
            exclude: Vec::new(),
            exclude_prefixes: Vec::new(),
            threads: default_threads(),
        }
    }
}

/// Half the cores, capped at 4.
///
/// The walk is syscall-bound — measured 1.28s system of a 1.53s total — so
/// threads do help, but this is a background service that must never be the
/// reason an interactive process stutters. Throughput past 4 threads mostly
/// buys contention on the inode cache.
pub fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() / 2).clamp(1, 4))
        .unwrap_or(1)
}

#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    pub n_dirs: i64,
    pub n_files: i64,
    pub n_entities: i64,
    pub n_hardlinks_deduped: i64,
    pub n_errors: i64,
    /// A few of the paths behind `n_errors`, for a message someone can act on.
    ///
    /// A count alone is not actionable: "3 errors" could be anything, while
    /// "cannot read /mnt/nextcloud/data" names the permission to fix. Capped,
    /// because a scan that cannot read anything must not turn into a log
    /// entry the size of the filesystem.
    pub unreadable: Vec<PathBuf>,
    /// Mount points not descended into because of their filesystem type, or
    /// because they duplicate a subtree reached through another mount. These
    /// are the uninteresting skips — virtual filesystems and snap images —
    /// and on a scan of `/` there are dozens.
    pub skipped_mounts: Vec<PathBuf>,
    /// Directories not descended into because they are on another device.
    ///
    /// Tracked separately because these are the *interesting* ones: a data
    /// drive mounted below the root looks exactly like this, and its bytes
    /// are silently absent from the total. Previously this was recorded
    /// nowhere, which made "no mount was skipped" a thing duTime could say
    /// while a whole filesystem sat unscanned under the root.
    pub other_filesystems: Vec<PathBuf>,
    /// The filesystem type the root sits on, from `/proc/self/mountinfo`.
    ///
    /// Recorded because it changes what an unreadable path *means*, and so
    /// what to do about one. See [`crate::scan::mounts::is_server_authorized`].
    pub root_fstype: Option<String>,
}

/// Enough crossings to name the drive; not so many that a nest of mounts
/// fills the log.
const MAX_OTHER_FS: usize = 16;

/// The path an ignore walk error refers to, if it names one.
///
/// `ignore::Error` nests: a `WithDepth` wraps a `WithPath` wraps the I/O
/// error, and only the middle layer carries the path we need.
fn err_path(e: &ignore::Error) -> Option<&Path> {
    match e {
        ignore::Error::WithPath { path, .. } => Some(path),
        ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
            err_path(err)
        }
        _ => None,
    }
}

/// How many example paths to keep out of an arbitrarily large failure set.
const MAX_UNREADABLE_EXAMPLES: usize = 8;

#[derive(Debug)]
pub struct ScanResult {
    pub tree: Tree,
    pub stats: ScanStats,
}

/// Walk `opts.root` and build a tree of exclusive sizes.
pub fn scan(opts: &ScanOptions) -> anyhow::Result<ScanResult> {
    let root = opts
        .root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("cannot access root {}: {e}", opts.root.display()))?;
    let root_meta = std::fs::symlink_metadata(&root)?;
    let root_dev = root_meta.dev();

    let mounts = MountTable::load();
    let root_fstype =
        mounts.as_ref().ok().and_then(|mt| mt.find_mount_for(&root)).map(|e| e.fstype.clone());
    let skip: std::collections::HashSet<PathBuf> = if opts.one_filesystem {
        mounts.as_ref().map(|mt| mt.skip_set(root_dev, &root)).unwrap_or_default()
    } else {
        Default::default()
    };

    // Keep only absolute excludes that fall strictly inside this root.
    //
    // A prefix that *contains* the root is deliberately ignored: if someone
    // configures /snap/foo as a root, they have explicitly asked for it, and
    // silently returning an empty tree would be worse than useless.
    let prefixes: Vec<PathBuf> = opts
        .exclude_prefixes
        .iter()
        .filter(|p| p.starts_with(&root) && p.as_path() != root)
        .cloned()
        .collect();

    let matcher = build_matcher(&root, &opts.exclude)?;
    let w = walk(&root, root_dev, opts, &skip, &matcher, &prefixes)?;
    let mut out = assemble(&root, w.raw, opts, skip);
    out.stats.n_errors = w.n_errors;
    out.stats.unreadable = w.unreadable;
    out.stats.other_filesystems = w.other_filesystems;
    out.stats.root_fstype = root_fstype;
    Ok(out)
}

/// What one walk produced: the entries, and everything it could not reach.
///
/// A struct rather than a tuple because three of the four are "things that
/// went unseen", and at a call site `(raw, n, a, b)` gives no clue which
/// `Vec<PathBuf>` is the unreadable paths and which is the crossed mounts.
struct WalkOutput {
    raw: Vec<RawEntry>,
    n_errors: i64,
    /// Paths that could not be read; their contents are missing from `raw`.
    unreadable: Vec<PathBuf>,
    /// Mount points on another device, not descended into.
    other_filesystems: Vec<PathBuf>,
}

/// Build a gitignore matcher where a bare pattern means *exclude*.
///
/// We deliberately never read `.gitignore` files off disk. For a disk-usage
/// tracker that would be actively wrong: `target/`, `node_modules/` and build
/// outputs are precisely the things you are trying to find.
fn build_matcher(root: &Path, patterns: &[String]) -> anyhow::Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        let line = escape_leading_hash(p);
        b.add_line(None, &line)
            .map_err(|e| anyhow::anyhow!("bad exclude pattern {p:?}: {e}"))?;
    }
    Ok(b.build()?)
}

/// Let a pattern start with `#`.
///
/// In a `.gitignore` file a leading `#` marks a comment, and the borrowed
/// syntax brings that with it — so `exclude = ["#recycle/"]` parses as a
/// comment and silently excludes nothing. Synology shares carry a `#recycle`
/// directory at the top of every one of them, so this is not a corner case;
/// it is the first thing anyone with a NAS tries to exclude.
///
/// The escape is `\#`, which nobody should have to know. In a list where
/// every entry *is* a pattern there is no such thing as a comment, so a
/// leading `#` can only have been meant literally.
///
/// `!` is left alone: it means negation, it is documented, and unlike a
/// comment it does something.
fn escape_leading_hash(pattern: &str) -> String {
    match pattern.strip_prefix('#') {
        Some(rest) => format!("\\#{rest}"),
        None => pattern.to_string(),
    }
}

fn walk(
    root: &Path,
    root_dev: u64,
    opts: &ScanOptions,
    skip: &std::collections::HashSet<PathBuf>,
    matcher: &Gitignore,
    prefixes: &[PathBuf],
) -> anyhow::Result<WalkOutput> {
    let (tx, rx) = mpsc::channel::<RawEntry>();
    let collector = std::thread::spawn(move || rx.into_iter().collect::<Vec<_>>());

    let mut wb = WalkBuilder::new(root);
    wb.hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .same_file_system(opts.one_filesystem)
        .threads(opts.threads)
        .skip_stdout(true);

    let n_errors = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0));
    let unreadable = std::sync::Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
    let other_fs = std::sync::Arc::new(std::sync::Mutex::new(Vec::<PathBuf>::new()));
    {
        let root = root.to_path_buf();
        wb.build_parallel().run(|| {
            let tx = tx.clone();
            let root = root.clone();
            let skip = skip.clone();
            let matcher = matcher.clone();
            let prefixes = prefixes.to_vec();
            let n_errors = n_errors.clone();
            let unreadable = unreadable.clone();
            let other_fs = other_fs.clone();
            Box::new(move |res| {
                use ignore::WalkState;
                let entry = match res {
                    Ok(e) => e,
                    Err(e) => {
                        n_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // Keep the first few paths. Everything under an
                        // unreadable directory is silently absent from the
                        // totals, so the one thing a scan must not do is
                        // report a smaller number without saying why.
                        if let Some(p) = err_path(&e) {
                            let mut v = unreadable.lock().unwrap();
                            if v.len() < MAX_UNREADABLE_EXAMPLES {
                                v.push(p.to_path_buf());
                            }
                        }
                        return WalkState::Continue;
                    }
                };
                let path = entry.path();

                // Never descend into a hazardous or duplicative mount point.
                if skip.contains(path) {
                    return WalkState::Skip;
                }
                if path != root && prefixes.iter().any(|p| path.starts_with(p)) {
                    return WalkState::Skip;
                }

                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                if path != root && matcher.matched(path, is_dir).is_ignore() {
                    return if is_dir { WalkState::Skip } else { WalkState::Continue };
                }

                let meta = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => {
                        n_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return WalkState::Continue;
                    }
                };

                // `same_file_system` on the walker stops descent, but the
                // mountpoint directory itself is still yielded; drop it so its
                // inode isn't attributed to this filesystem.
                if opts.one_filesystem && meta.dev() != root_dev && path != root {
                    // Record it. Declining to cross a filesystem boundary is
                    // correct, but it leaves that filesystem's bytes out of
                    // the total, and nobody can reconcile a number against
                    // `du` without being told.
                    let mut v = other_fs.lock().unwrap();
                    if v.len() < MAX_OTHER_FS && !v.iter().any(|p| path.starts_with(p)) {
                        v.push(path.to_path_buf());
                    }
                    return WalkState::Skip;
                }

                let ft = meta.file_type();
                let kind = if ft.is_dir() {
                    Kind::Dir
                } else if ft.is_file() {
                    Kind::File
                } else if ft.is_symlink() {
                    Kind::Symlink
                } else {
                    Kind::Other
                };

                let rel = match path.strip_prefix(&root) {
                    Ok(r) => r.iter().map(|c| c.to_os_string()).collect(),
                    Err(_) => return WalkState::Continue,
                };

                let _ = tx.send(RawEntry {
                    rel,
                    kind,
                    bytes: meta.size() as i64,
                    blocks: meta.blocks() as i64 * 512,
                    ino: meta.ino(),
                    dev: meta.dev(),
                    nlink: meta.nlink(),
                });
                WalkState::Continue
            })
        });
    }
    drop(tx);
    let raw = collector.join().map_err(|_| anyhow::anyhow!("collector thread panicked"))?;
    let take = |a: std::sync::Arc<std::sync::Mutex<Vec<PathBuf>>>| {
        std::sync::Arc::try_unwrap(a).map(|m| m.into_inner().unwrap()).unwrap_or_default()
    };
    let mut other_filesystems = take(other_fs);
    other_filesystems.sort();
    Ok(WalkOutput {
        raw,
        n_errors: n_errors.load(std::sync::atomic::Ordering::Relaxed),
        unreadable: take(unreadable),
        other_filesystems,
    })
}

/// Turn raw walk output into a tree of exclusive sizes.
fn assemble(
    _root: &Path,
    raw: Vec<RawEntry>,
    opts: &ScanOptions,
    skip: std::collections::HashSet<PathBuf>,
) -> ScanResult {
    let mut stats = ScanStats {
        skipped_mounts: {
            let mut v: Vec<_> = skip.into_iter().collect();
            v.sort();
            v
        },
        ..Default::default()
    };

    // ── Hardlink dedup, deterministically ─────────────────────────────────
    //
    // `du` counts a multiply-linked inode once. Which link "wins" would
    // otherwise depend on the order the parallel walk happened to finish in,
    // so the same tree would report different per-directory sizes between two
    // scans and manufacture phantom deltas. `raw` is sorted by relative path,
    // so first-wins here is stable across runs and across thread counts.
    // Only multiply-linked entries need ordering, and on a real home directory
    // that is 23k of 883k entries — so sort those indices rather than paying
    // an O(n log n) path comparison over the whole walk.
    let mut deduped: Vec<bool> = vec![false; raw.len()];
    let mut linked: Vec<usize> = (0..raw.len())
        .filter(|&i| raw[i].nlink > 1 && raw[i].kind != Kind::Dir)
        .collect();
    linked.sort_by(|&a, &b| raw[a].rel.cmp(&raw[b].rel));

    let mut seen: HashMap<(u64, u64), ()> = HashMap::new();
    for i in linked {
        if seen.insert((raw[i].dev, raw[i].ino), ()).is_some() {
            deduped[i] = true;
            stats.n_hardlinks_deduped += 1;
        }
    }

    // ── Build the dictionary ──────────────────────────────────────────────
    let mut tree = Tree::new();
    tree.add_root(OsString::from(""), Kind::Dir);

    // Directories first, so every parent exists before any child is attached.
    //
    // Note the asymmetry, which is `du`'s and is genuinely surprising:
    // `du --apparent-size` does NOT count a directory's own st_size, while
    // ordinary (allocated) `du` DOES count its st_blocks. Verified against GNU
    // coreutils: a directory holding one 5-byte file reports 5 bytes apparent
    // but 8192 bytes allocated (4096 for the directory inode + 4096 for the
    // file). Adding the directory's st_size to the apparent total would
    // overstate every root by 4 KiB per directory — 69,632 bytes on a
    // 17-directory fixture, and ~400 MB on a 100k-directory home tree.
    for e in raw.iter().filter(|e| e.kind == Kind::Dir) {
        let idx = ensure_path(&mut tree, &e.rel, Kind::Dir);
        tree.own_blocks[idx as usize] += e.blocks;
        stats.n_dirs += 1;
    }

    for (i, e) in raw.iter().enumerate() {
        if e.kind == Kind::Dir {
            continue;
        }
        if e.kind == Kind::File {
            stats.n_files += 1;
        }

        let (bytes, blocks) = if deduped[i] { (0, 0) } else { (e.bytes, e.blocks) };

        // Promote large regular files to entities of their own. A deduped
        // hardlink is never promoted: it contributes nothing, so giving it a
        // node would add a permanently-zero series to the dictionary.
        let promote =
            e.kind == Kind::File && !deduped[i] && e.bytes >= opts.track_file_min_bytes;

        if promote {
            let idx = ensure_path(&mut tree, &e.rel, Kind::File);
            tree.own_bytes[idx as usize] = bytes;
            tree.own_blocks[idx as usize] = blocks;
            stats.n_entities += 1;
        } else {
            // Fold into the parent directory's exclusive size.
            let parent = match e.rel.split_last() {
                Some((_, head)) => ensure_path(&mut tree, head, Kind::Dir),
                None => 0,
            };
            tree.own_bytes[parent as usize] += bytes;
            tree.own_blocks[parent as usize] += blocks;
            if e.kind == Kind::File {
                tree.own_files[parent as usize] += 1;
            }
        }
    }

    stats.n_entities += stats.n_dirs;
    ScanResult { tree, stats }
}

fn ensure_path(tree: &mut Tree, rel: &[OsString], kind: Kind) -> NodeIdx {
    let mut cur: NodeIdx = 0;
    for (i, c) in rel.iter().enumerate() {
        let k = if i + 1 == rel.len() { kind } else { Kind::Dir };
        cur = tree.get_or_insert(cur, c, k);
    }
    cur
}

/// Flatten a scanned tree into [`Entity`] values, for tests and the CLI.
pub fn entities(tree: &Tree) -> Vec<Entity> {
    (0..tree.len() as NodeIdx)
        .map(|i| Entity {
            rel: tree.rel_path(i),
            kind: tree.kind[i as usize],
            own_bytes: tree.own_bytes[i as usize],
            own_blocks: tree.own_blocks[i as usize],
            own_files: tree.own_files[i as usize],
            ino: None,
            dev: None,
            flags: flags::BORN,
        })
        .collect()
}

#[cfg(test)]
mod crossing_tests {
    use super::*;

    /// An ordinary single-filesystem tree must report no crossings.
    ///
    /// The cross-device branch is the one that drives a "your data is behind
    /// a mount" message, so it has to stay quiet when there is no mount. The
    /// positive case cannot be built without privilege and was verified
    /// instead against a live scan of `/`, which correctly reported exactly
    /// one crossing (`/boot/efi`, vfat, its own device) out of 90 mounts.
    #[test]
    fn a_single_filesystem_tree_reports_no_crossings() {
        let dir = tempfile::Builder::new().prefix("dutime-xfs-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/f.bin"), vec![b'x'; 2 << 20]).unwrap();

        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();

        assert!(
            r.stats.other_filesystems.is_empty(),
            "reported a filesystem crossing where there is none: {:?}",
            r.stats.other_filesystems
        );
        assert_eq!(r.stats.n_errors, 0);
        // And it did actually walk the tree, so the assertion above is not
        // passing merely because nothing happened.
        assert!(r.tree.len() >= 4, "only found {} entities", r.tree.len());
    }
}

#[cfg(test)]
mod exclude_tests {
    use super::*;

    /// A pattern beginning with `#` must exclude something.
    ///
    /// Gitignore syntax reads a leading `#` as a comment, so
    /// `exclude = ["#recycle/"]` used to parse as a comment and quietly
    /// exclude nothing — with no error, and a scan that looked fine while
    /// still walking the directory you had asked it to skip. Synology puts a
    /// `#recycle` at the top of every shared folder, so this is the first
    /// thing a NAS owner tries.
    #[test]
    fn a_pattern_starting_with_hash_is_a_pattern_not_a_comment() {
        let dir = tempfile::Builder::new().prefix("dutime-hash-").tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for d in ["#recycle", "keep"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("f.bin"), vec![b'x'; 2 << 20]).unwrap();
        }

        let total = |patterns: &[&str]| {
            let mut o = ScanOptions::new(&root);
            o.threads = 2;
            o.exclude = patterns.iter().map(|s| s.to_string()).collect();
            let r = scan(&o).unwrap();
            r.tree.rollup().bytes[0]
        };

        let all = total(&[]);
        let excluded = total(&["#recycle/"]);
        assert!(excluded < all, "a leading-# pattern excluded nothing: {excluded} vs {all}");
        // And it excluded the right one.
        assert_eq!(excluded, total(&["\\#recycle/"]), "differs from the escaped form");
        assert_eq!(all, total(&["keep/"]) + excluded - total(&["#recycle/", "keep/"]));
    }

    /// `!` means negation and has to keep meaning it.
    #[test]
    fn negation_is_left_alone() {
        assert_eq!(escape_leading_hash("!keep/"), "!keep/");
        assert_eq!(escape_leading_hash("**/*.sock"), "**/*.sock");
        assert_eq!(escape_leading_hash("#recycle/"), "\\#recycle/");
        // Only the leading one; a `#` inside a name is not special anyway.
        assert_eq!(escape_leading_hash("a#b"), "a#b");
    }
}
