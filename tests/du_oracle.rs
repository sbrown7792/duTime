//! The gate.
//!
//! duTime's entire value rests on its numbers being trustworthy, and there is
//! exactly one authority on what a directory's size is: `du`. These tests
//! assert byte-for-byte agreement on a fixture tree built to contain every
//! accounting hazard we know of — hardlinks, sparse files, symlinks, empty
//! directories, non-UTF-8 filenames, and files straddling the promotion
//! threshold.
//!
//! If these fail, nothing downstream is worth debugging.

use dutime::model::Kind;
use dutime::scan::walker::{ScanOptions, scan};
use std::fs;
use std::io::{Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

/// `du -sxb <dir>` — apparent size (sum of st_size) in bytes.
fn du_apparent(dir: &Path) -> i64 {
    du(&["-sxb"], dir)
}

/// `du -sx --block-size=1 <dir>` — allocated size (st_blocks * 512) in bytes.
///
/// `--block-size=1` rather than plain `du -sx`, whose KiB output rounds and
/// would make an exact comparison impossible.
fn du_allocated(dir: &Path) -> i64 {
    du(&["-sx", "--block-size=1"], dir)
}

fn du(args: &[&str], dir: &Path) -> i64 {
    let out = Command::new("du")
        .args(args)
        .arg(dir)
        .output()
        .expect("du must be installed");
    assert!(
        out.status.success(),
        "du failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let s = String::from_utf8_lossy(&out.stdout);
    s.split_whitespace()
        .next()
        .and_then(|t| t.parse().ok())
        .unwrap_or_else(|| panic!("unparseable du output: {s:?}"))
}

/// Build a tree containing every accounting hazard we care about.
///
/// `cross_dir_hardlink` adds a link to an inode that already lives in another
/// directory. That case is real and must be handled, but it makes per-directory
/// comparison against an independent `du` invocation meaningless — see
/// [`every_subdirectory_matches_du`] and
/// [`cross_directory_hardlink_is_credited_once_at_root_scope`].
fn build_fixture(root: &Path, cross_dir_hardlink: bool) {
    let w = |p: &Path, n: usize| {
        let mut f = fs::File::create(p).unwrap();
        f.write_all(&vec![b'x'; n]).unwrap();
    };

    fs::create_dir_all(root.join("a/b/c")).unwrap();
    fs::create_dir_all(root.join("a/empty")).unwrap();
    fs::create_dir_all(root.join("deep/1/2/3/4/5/6/7/8/9")).unwrap();

    // Straddle the 1 MiB promotion threshold in both directions, and land
    // exactly on it — an off-by-one in the `>=` comparison would move bytes
    // between a parent and a child without changing the total, so the totals
    // test alone would not catch it. `threshold_invariance` does.
    w(&root.join("a/just_under.bin"), (1 << 20) - 1);
    w(&root.join("a/exactly.bin"), 1 << 20);
    w(&root.join("a/just_over.bin"), (1 << 20) + 1);
    w(&root.join("a/b/small.txt"), 17);
    w(&root.join("a/b/c/tiny"), 1);
    w(&root.join("deep/1/2/3/4/5/6/7/8/9/bottom.bin"), 4096);

    // A zero-length file: real, counted as an inode, contributes no bytes.
    fs::File::create(root.join("a/b/zero")).unwrap();

    // Hardlinks. `du` credits the inode once; we must pick the same winner it
    // does *and* pick it deterministically across runs.
    fs::create_dir_all(root.join("links")).unwrap();
    w(&root.join("links/original.bin"), 300_000);
    fs::hard_link(root.join("links/original.bin"), root.join("links/hard1.bin")).unwrap();
    fs::hard_link(root.join("links/original.bin"), root.join("links/hard2.bin")).unwrap();
    if cross_dir_hardlink {
        // A hardlink in a *different* directory, so dedup has to be stable
        // across the whole tree and not merely within one directory listing.
        fs::hard_link(root.join("links/original.bin"), root.join("a/b/hard3.bin")).unwrap();
    }

    // Sparse file: st_size is 8 MiB but almost no blocks are allocated. This is
    // the case where apparent and allocated must diverge sharply.
    {
        let mut f = fs::File::create(root.join("sparse.img")).unwrap();
        f.seek(SeekFrom::Start(8 << 20)).unwrap();
        f.write_all(b"end").unwrap();
    }

    // Symlinks, including a dangling one and one pointing at a directory.
    // `du` counts the link's own st_size (the target string length) and never
    // follows it.
    std::os::unix::fs::symlink("a/b/small.txt", root.join("link_to_file")).unwrap();
    std::os::unix::fs::symlink("a/b/c", root.join("link_to_dir")).unwrap();
    std::os::unix::fs::symlink("nowhere/at/all", root.join("dangling")).unwrap();

    // Non-UTF-8 filenames. Linux filenames are arbitrary bytes; a tool that
    // assumes UTF-8 corrupts them or panics.
    let weird = std::ffi::OsStr::from_bytes(b"weird-\xff\xfe-name.bin");
    w(&root.join(weird), 2048);
    let weird_dir = std::ffi::OsStr::from_bytes(b"dir-\x80\x81");
    fs::create_dir_all(root.join(weird_dir)).unwrap();
    w(&root.join(weird_dir).join("inside.bin"), 5000);

    // Filenames with spaces, newlines and quotes.
    w(&root.join("a/with space.bin"), 123);
    w(&root.join("a/with'quote.bin"), 45);
}

fn fixture() -> tempfile::TempDir {
    fixture_with(true)
}

fn fixture_with(cross_dir_hardlink: bool) -> tempfile::TempDir {
    fs::create_dir_all("target/fixtures").unwrap();
    let td = tempfile::Builder::new()
        .prefix("dutime-oracle-")
        .tempdir_in("target/fixtures")
        .unwrap();
    build_fixture(td.path(), cross_dir_hardlink);
    td
}

#[test]
fn matches_du_apparent_size_exactly() {
    let td = fixture();
    let opts = ScanOptions::new(td.path());
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();

    let ours = roll.bytes[0];
    let theirs = du_apparent(td.path());
    assert_eq!(
        ours, theirs,
        "apparent size mismatch: duTime={ours} du -sxb={theirs} (delta {})",
        ours - theirs
    );
}

#[test]
fn matches_du_allocated_size_exactly() {
    let td = fixture();
    let opts = ScanOptions::new(td.path());
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();

    let ours = roll.blocks[0];
    let theirs = du_allocated(td.path());
    assert_eq!(
        ours, theirs,
        "allocated size mismatch: duTime={ours} du -sx --block-size=1={theirs} (delta {})",
        ours - theirs
    );
}

/// Every subdirectory must match `du` too, not just the root.
///
/// A root-only check can pass while bytes are attributed to the wrong
/// directory — precisely the failure that would make "biggest gainers" point at
/// the wrong culprit.
///
/// Uses the fixture *without* a cross-directory hardlink, because `du <subdir>`
/// deduplicates only within that one invocation: run on `links/` alone it has
/// never seen `a/b/hard3.bin`, so it counts the shared inode that duTime
/// credited elsewhere. The two tools are answering different questions at
/// subtree scope, and only the root-scope comparison is a fair test.
#[test]
fn every_subdirectory_matches_du() {
    let td = fixture_with(false);
    let opts = ScanOptions::new(td.path());
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();

    let mut checked = 0;
    for i in 0..r.tree.len() {
        if r.tree.kind[i] != Kind::Dir {
            continue;
        }
        let rel = r.tree.rel_path(i as u32);
        let mut p = td.path().to_path_buf();
        for c in &rel {
            p.push(c);
        }
        let ours = roll.bytes[i];
        let theirs = du_apparent(&p);
        assert_eq!(ours, theirs, "apparent mismatch at {}", p.display());

        let ours_b = roll.blocks[i];
        let theirs_b = du_allocated(&p);
        assert_eq!(ours_b, theirs_b, "allocated mismatch at {}", p.display());
        checked += 1;
    }
    assert!(checked >= 8, "expected to check several dirs, only did {checked}");
}

/// The promotion threshold moves bytes between a parent and a child entity but
/// must never change any inclusive total.
///
/// This is the highest-probability silent correctness bug in the whole design:
/// when a file crosses `track_file_min_bytes`, its bytes leave the parent's
/// exclusive size and appear as a new entity in the same scan. If those two
/// movements don't cancel exactly, every ancestor's history drifts — silently,
/// and forever.
#[test]
fn threshold_invariance() {
    let td = fixture();
    let truth = du_apparent(td.path());
    let truth_alloc = du_allocated(td.path());

    for threshold in [0, 1, 1024, (1 << 20) - 1, 1 << 20, (1 << 20) + 1, i64::MAX] {
        let mut opts = ScanOptions::new(td.path());
        opts.track_file_min_bytes = threshold;
        let r = scan(&opts).expect("scan");
        let roll = r.tree.rollup();
        assert_eq!(
            roll.bytes[0], truth,
            "apparent total changed at threshold {threshold}"
        );
        assert_eq!(
            roll.blocks[0], truth_alloc,
            "allocated total changed at threshold {threshold}"
        );
    }
}

/// The same tree scanned with different thread counts must produce identical
/// per-directory numbers.
///
/// Hardlink dedup picks one link to credit. If the winner depends on the order
/// the parallel walk finished in, two consecutive scans of an unchanged tree
/// would disagree and duTime would invent growth that never happened.
#[test]
fn hardlink_dedup_is_deterministic_across_thread_counts() {
    let td = fixture();
    let mut baseline: Option<Vec<(Vec<std::ffi::OsString>, i64)>> = None;

    for threads in [1, 2, 4, 8] {
        let mut opts = ScanOptions::new(td.path());
        opts.threads = threads;
        let r = scan(&opts).expect("scan");
        let roll = r.tree.rollup();

        let mut snapshot: Vec<(Vec<std::ffi::OsString>, i64)> = (0..r.tree.len())
            .map(|i| (r.tree.rel_path(i as u32), roll.bytes[i]))
            .collect();
        snapshot.sort();

        match &baseline {
            None => baseline = Some(snapshot),
            Some(b) => assert_eq!(
                *b, snapshot,
                "per-path sizes differ at {threads} threads — dedup is order-dependent"
            ),
        }
    }
    assert!(baseline.is_some());
}

/// Sparse files must show a large apparent size and a tiny allocated size.
#[test]
fn sparse_file_diverges_apparent_from_allocated() {
    let td = fixture();
    let opts = ScanOptions::new(td.path());
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();

    let idx = (0..r.tree.len())
        .find(|&i| r.tree.name[i] == std::ffi::OsString::from("sparse.img"))
        .expect("sparse.img should be promoted to its own entity (8 MiB > 1 MiB)");

    assert!(roll.bytes[idx] > 8 << 20, "apparent should exceed 8 MiB");
    assert!(
        roll.blocks[idx] < 1 << 20,
        "allocated should be tiny for a sparse file, got {}",
        roll.blocks[idx]
    );
}

/// Non-UTF-8 filenames must survive the walk intact.
#[test]
fn preserves_non_utf8_filenames() {
    let td = fixture();
    // Threshold 0 promotes every file to an entity, so the dictionary has to
    // round-trip the raw filename bytes rather than folding them into a parent.
    let mut opts = ScanOptions::new(td.path());
    opts.track_file_min_bytes = 0;
    let r = scan(&opts).expect("scan");

    let want = std::ffi::OsStr::from_bytes(b"weird-\xff\xfe-name.bin");
    assert!(
        (0..r.tree.len()).any(|i| r.tree.name[i] == want),
        "non-UTF-8 filename was lost or mangled"
    );
    let want_dir = std::ffi::OsStr::from_bytes(b"dir-\x80\x81");
    assert!(
        (0..r.tree.len()).any(|i| r.tree.name[i] == want_dir),
        "non-UTF-8 directory name was lost or mangled"
    );
}

/// A hardlinked inode reachable from two directories is counted exactly once
/// at root scope, and always credited to the same place.
///
/// `du` credits whichever link its traversal reaches first, which varies with
/// directory order. duTime instead credits the lowest-sorting path. Both agree
/// on the root total — the only number that is scope-independent — but they can
/// disagree about *which* directory owns the bytes. duTime's choice is the
/// deliberate one: a stable winner means an unchanged tree reports unchanged
/// sizes, instead of manufacturing a spurious gain in one directory and an
/// equal loss in another every time the walk order shifts.
#[test]
fn cross_directory_hardlink_is_credited_once_at_root_scope() {
    let td = fixture_with(true);
    let opts = ScanOptions::new(td.path());
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();

    assert_eq!(
        roll.bytes[0],
        du_apparent(td.path()),
        "root total must match du even with a cross-directory hardlink"
    );
    assert_eq!(r.stats.n_hardlinks_deduped, 3, "3 of the 4 links should be deduped");

    // Re-scan with every file promoted so each individual link is its own
    // entity and we can see exactly which one was credited.
    let mut opts = ScanOptions::new(td.path());
    opts.track_file_min_bytes = 0;
    let r = scan(&opts).expect("scan");
    let roll = r.tree.rollup();
    let at = |rel: &str| {
        let parts: Vec<std::ffi::OsString> =
            rel.split('/').map(std::ffi::OsString::from).collect();
        let idx = r.tree.resolve(&parts).unwrap_or_else(|| panic!("{rel} not found"));
        roll.bytes[idx as usize]
    };

    // a/b/hard3.bin sorts before links/*, so it is the credited link.
    assert_eq!(at("a/b/hard3.bin"), 300_000, "lowest-sorting link should be credited");
    assert_eq!(at("links"), 0, "links/ holds only deduped copies");

    // The deduped links get no entity at all, rather than a permanently-zero
    // one. This matches `du -a`, which omits them from its listing entirely
    // (verified against GNU coreutils) and keeps the dictionary from
    // accumulating dead series for every hardlink on the system.
    for gone in ["links/original.bin", "links/hard1.bin", "links/hard2.bin"] {
        let parts: Vec<std::ffi::OsString> =
            gone.split('/').map(std::ffi::OsString::from).collect();
        assert!(r.tree.resolve(&parts).is_none(), "{gone} should have no entity");
    }
}
