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
    /// Gitignore-syntax patterns. A bare pattern excludes; a `!` prefix
    /// re-includes. Note this is gitignore semantics, *not* ripgrep `--glob`
    /// semantics, where a bare pattern would whitelist.
    pub exclude: Vec<String>,
    pub threads: usize,
}

impl ScanOptions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            track_file_min_bytes: 1 << 20,
            one_filesystem: true,
            exclude: Vec::new(),
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
    pub skipped_mounts: Vec<PathBuf>,
}

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

    let skip: std::collections::HashSet<PathBuf> = if opts.one_filesystem {
        MountTable::load()
            .map(|mt| mt.skip_set(root_dev, &root))
            .unwrap_or_default()
    } else {
        Default::default()
    };

    let matcher = build_matcher(&root, &opts.exclude)?;
    let (raw, n_errors) = walk(&root, root_dev, opts, &skip, &matcher)?;
    let mut out = assemble(&root, raw, opts, skip);
    out.stats.n_errors = n_errors;
    Ok(out)
}

/// Build a gitignore matcher where a bare pattern means *exclude*.
///
/// We deliberately never read `.gitignore` files off disk. For a disk-usage
/// tracker that would be actively wrong: `target/`, `node_modules/` and build
/// outputs are precisely the things you are trying to find.
fn build_matcher(root: &Path, patterns: &[String]) -> anyhow::Result<Gitignore> {
    let mut b = GitignoreBuilder::new(root);
    for p in patterns {
        b.add_line(None, p)
            .map_err(|e| anyhow::anyhow!("bad exclude pattern {p:?}: {e}"))?;
    }
    Ok(b.build()?)
}

fn walk(
    root: &Path,
    root_dev: u64,
    opts: &ScanOptions,
    skip: &std::collections::HashSet<PathBuf>,
    matcher: &Gitignore,
) -> anyhow::Result<(Vec<RawEntry>, i64)> {
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
    {
        let root = root.to_path_buf();
        wb.build_parallel().run(|| {
            let tx = tx.clone();
            let root = root.clone();
            let skip = skip.clone();
            let matcher = matcher.clone();
            let n_errors = n_errors.clone();
            Box::new(move |res| {
                use ignore::WalkState;
                let entry = match res {
                    Ok(e) => e,
                    Err(_) => {
                        n_errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return WalkState::Continue;
                    }
                };
                let path = entry.path();

                // Never descend into a hazardous or duplicative mount point.
                if skip.contains(path) {
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
    Ok((raw, n_errors.load(std::sync::atomic::Ordering::Relaxed)))
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
