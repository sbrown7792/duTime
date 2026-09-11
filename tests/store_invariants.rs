//! Invariants of the change-only event store.
//!
//! The scanner is checked against `du` in `du_oracle.rs`. These tests check the
//! other half: that history reconstructed from sparse deltas equals the truth
//! the scanner saw, under births, deaths, mass deletion, and files crossing the
//! tracking threshold.
//!
//! The headline property, stated once in `schema.sql` and asserted here:
//!
//! ```text
//! SUM(d_bytes) over subtree(A) in (S1, S2]  ==  incl(A, S2) - incl(A, S1)
//! ```

use dutime::model::ScanId;
use dutime::scan::walker::{ScanOptions, scan};
use dutime::store::commit::{CommitOptions, CommitStats, commit_scan, verify_current_size};
use dutime::store::query::Extreme;
use dutime::store::{Store, query};
use std::fs;
use std::io::Write;

struct Harness {
    store: Store,
    dir: tempfile::TempDir,
    root_id: i64,
    clock: i64,
    opts: CommitOptions,
}

impl Harness {
    fn new() -> Self {
        Self::with_opts(CommitOptions::default())
    }

    fn with_opts(opts: CommitOptions) -> Self {
        fs::create_dir_all("target/fixtures").unwrap();
        let dir = tempfile::Builder::new()
            .prefix("dutime-store-")
            .tempdir_in("target/fixtures")
            .unwrap();
        let store = Store::open_in_memory().unwrap();
        let canon = dir.path().canonicalize().unwrap();
        let root_id = store.ensure_root(&canon).unwrap();
        Self { store, dir, root_id, clock: 1_700_000_000, opts }
    }

    fn path(&self) -> std::path::PathBuf {
        self.dir.path().canonicalize().unwrap()
    }

    /// Walk and commit, returning the commit stats and the truth the scanner
    /// saw, so tests can compare stored history against ground truth.
    fn snapshot(&mut self) -> (CommitStats, i64, i64) {
        self.snapshot_with(1 << 20)
    }

    fn snapshot_with(&mut self, min_file: i64) -> (CommitStats, i64, i64) {
        let root = self.path();
        let mut o = ScanOptions::new(&root);
        o.track_file_min_bytes = min_file;
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        let (truth_b, truth_k) = (roll.bytes[0], roll.blocks[0]);

        self.clock += 300;
        let st = commit_scan(
            &mut self.store,
            self.root_id,
            &root,
            &r.tree,
            &roll,
            &r.stats,
            self.clock,
            1234,
            &self.opts,
        )
        .unwrap();
        (st, truth_b, truth_k)
    }

    fn root_path_id(&self) -> i64 {
        self.store
            .conn
            .query_row(
                "SELECT path_id FROM path WHERE root_id = ?1 AND parent_id IS NULL",
                [self.root_id],
                |r| r.get(0),
            )
            .unwrap()
    }

    fn scans(&self) -> Vec<ScanId> {
        let mut st = self
            .store
            .conn
            .prepare("SELECT scan_id FROM scan WHERE root_id = ?1 ORDER BY scan_id")
            .unwrap();
        let v = st.query_map([self.root_id], |r| r.get(0)).unwrap();
        v.collect::<Result<_, _>>().unwrap()
    }

    fn write(&self, rel: &str, n: usize) {
        let p = self.dir.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let mut f = fs::File::create(&p).unwrap();
        f.write_all(&vec![b'x'; n]).unwrap();
    }
}

/// A tree that has not changed must produce no events at all.
///
/// This is the one that keeps duTime honest. If an unchanged tree generates
/// events, every "biggest gainer" list fills with noise, the database grows
/// without bound, and the product's central claim — that it can tell you what
/// actually changed — is false.
#[test]
fn unchanged_tree_produces_no_events() {
    let mut h = Harness::new();
    h.write("a/big.bin", 2 << 20);
    h.write("a/b/small.txt", 100);
    h.write("c/other.bin", 3 << 20);

    let (first, _, _) = h.snapshot();
    assert!(first.n_events > 0, "first scan must record a baseline");

    for round in 0..3 {
        let (st, _, _) = h.snapshot();
        assert_eq!(st.n_events, 0, "round {round}: unchanged tree emitted events");
        assert_eq!(st.n_born, 0);
        assert_eq!(st.n_gone, 0);
    }
}

/// The core invariant, across every pair of scans.
#[test]
fn subtree_delta_equals_inclusive_difference() {
    let mut h = Harness::with_opts(CommitOptions {
        checkpoint_every_scans: 3,
        checkpoint_min_bytes: 1 << 20,
    });
    h.write("a/big.bin", 2 << 20);
    h.write("a/b/mid.bin", 1 << 20);
    h.write("c/x.bin", 5 << 20);
    h.snapshot();

    h.write("a/big.bin", 6 << 20); // grow
    h.snapshot();

    h.write("a/b/new.bin", 4 << 20); // create
    h.snapshot();

    fs::remove_file(h.dir.path().join("c/x.bin")).unwrap(); // delete
    h.snapshot();

    h.write("a/b/mid.bin", 100); // shrink below threshold
    h.snapshot();

    let root = h.root_path_id();
    let scans = h.scans();
    for i in 0..scans.len() {
        for j in i..scans.len() {
            let (s1, s2) = (scans[i], scans[j]);
            let (d_b, d_k) = query::subtree_delta(&h.store, root, s1, s2).unwrap();
            let (a_b, a_k) = query::incl_at(&h.store, root, s1).unwrap();
            let (b_b, b_k) = query::incl_at(&h.store, root, s2).unwrap();
            assert_eq!(
                d_b,
                b_b - a_b,
                "bytes invariant broken between scans {s1} and {s2}"
            );
            assert_eq!(
                d_k,
                b_k - a_k,
                "blocks invariant broken between scans {s1} and {s2}"
            );
        }
    }
}

/// The fast checkpoint+delta path must agree with the slow, obviously-correct
/// replay at every point in history.
#[test]
fn fast_path_agrees_with_naive_oracle() {
    let mut h = Harness::with_opts(CommitOptions {
        checkpoint_every_scans: 2,
        checkpoint_min_bytes: 0,
    });
    let mut truths = Vec::new();

    h.write("x/a.bin", 1 << 20);
    truths.push(h.snapshot());
    h.write("x/b.bin", 3 << 20);
    truths.push(h.snapshot());
    h.write("x/a.bin", 7 << 20);
    truths.push(h.snapshot());
    fs::remove_dir_all(h.dir.path().join("x")).unwrap();
    h.write("y/c.bin", 2 << 20);
    truths.push(h.snapshot());
    h.write("y/d/e.bin", 9 << 20);
    truths.push(h.snapshot());

    let root = h.root_path_id();
    for (i, s) in h.scans().into_iter().enumerate() {
        let fast = query::incl_at(&h.store, root, s).unwrap();
        let naive = query::incl_at_naive(&h.store, root, s).unwrap();
        assert_eq!(fast, naive, "fast path disagrees with oracle at scan {s}");

        // ...and both must equal what the scanner actually measured.
        let (_, truth_b, truth_k) = truths[i];
        assert_eq!(fast, (truth_b, truth_k), "reconstruction != ground truth at scan {s}");
    }
}

/// A file crossing the tracking threshold must not perturb any total.
///
/// When a file grows past `track_file_min_bytes` it stops being folded into its
/// parent's exclusive size and becomes an entity of its own. Those two
/// movements — bytes out of the parent, bytes into a new child — happen in the
/// same scan and must cancel exactly. If they don't, every ancestor's history
/// drifts silently and permanently.
#[test]
fn threshold_crossing_deltas_cancel() {
    let mut h = Harness::with_opts(CommitOptions {
        checkpoint_every_scans: 1000,
        checkpoint_min_bytes: 0,
    });
    let threshold = 1 << 20;

    h.write("d/grower.bin", 1024); // well under
    let (_, t0, k0) = h.snapshot_with(threshold);

    h.write("d/grower.bin", threshold as usize - 1); // still under
    let (_, t1, k1) = h.snapshot_with(threshold);

    h.write("d/grower.bin", threshold as usize); // exactly at: promoted
    let (promo, t2, k2) = h.snapshot_with(threshold);

    h.write("d/grower.bin", 512); // back under: demoted
    let (demo, t3, k3) = h.snapshot_with(threshold);

    let root = h.root_path_id();
    let scans = h.scans();
    for (i, (truth_b, truth_k)) in [(t0, k0), (t1, k1), (t2, k2), (t3, k3)].iter().enumerate() {
        let got = query::incl_at(&h.store, root, scans[i]).unwrap();
        assert_eq!(
            got,
            (*truth_b, *truth_k),
            "total wrong at scan {} (promotion/demotion leaked bytes)",
            scans[i]
        );
    }

    // Promotion creates the child entity and shrinks the parent in one scan.
    assert!(promo.n_born >= 1, "promotion should create a new entity");
    // Demotion retires it again.
    assert!(demo.n_gone >= 1 || demo.n_events >= 1);

    // And the deltas must sum to the real change, not merely agree by accident
    // on the endpoints.
    let (d, _) = query::subtree_delta(&h.store, root, scans[1], scans[2]).unwrap();
    assert_eq!(d, t2 - t1, "promotion scan's deltas do not sum to the real change");
}

/// Removing a large subtree must cost one event, not one per descendant.
#[test]
fn mass_delete_emits_one_event() {
    let mut h = Harness::new();
    // 200 directories, each with a promoted file.
    for i in 0..200 {
        h.write(&format!("bulk/d{i}/f.bin", i = i), (1 << 20) + i);
    }
    h.write("keep/other.bin", 1 << 20);
    let (first, _, _) = h.snapshot();
    assert!(first.n_events > 400, "baseline should record every entity");

    let before = query::incl_at(&h.store, h.root_path_id(), *h.scans().last().unwrap()).unwrap();

    fs::remove_dir_all(h.dir.path().join("bulk")).unwrap();
    let (del, truth_b, truth_k) = h.snapshot();

    assert_eq!(
        del.n_subtree_gone, 1,
        "a 200-directory delete must collapse to a single SUBTREE_GONE event"
    );
    assert_eq!(
        del.n_events, 1,
        "expected exactly one event for the whole mass delete, got {}",
        del.n_events
    );

    let after = query::incl_at(&h.store, h.root_path_id(), *h.scans().last().unwrap()).unwrap();
    assert_eq!(after, (truth_b, truth_k), "size after mass delete is wrong");
    assert!(after.0 < before.0, "deleting 200 MB should reduce the total");
}

/// `current_size` is a materialization and must never drift from the events it
/// summarizes.
#[test]
fn current_size_matches_event_replay() {
    let mut h = Harness::new();
    h.write("a/one.bin", 2 << 20);
    h.snapshot();
    h.write("a/two.bin", 3 << 20);
    h.write("a/one.bin", 1 << 20);
    h.snapshot();
    fs::remove_file(h.dir.path().join("a/two.bin")).unwrap();
    h.snapshot();
    h.write("b/three.bin", 4 << 20);
    h.snapshot();

    let drifted = verify_current_size(&h.store, h.root_id).unwrap();
    assert!(drifted.is_empty(), "current_size drifted for path_ids {drifted:?}");
}

/// Biggest-gainers must name the directory that actually grew.
#[test]
fn gainers_identify_the_culprit() {
    let mut h = Harness::new();
    h.write("quiet/a.bin", 2 << 20);
    h.write("noisy/b.bin", 2 << 20);
    h.write("noisy/deep/c.bin", 2 << 20);
    h.snapshot();

    let s1 = *h.scans().last().unwrap();
    // Only this one grows, and by a lot.
    h.write("noisy/deep/c.bin", 50 << 20);
    h.snapshot();
    let s2 = *h.scans().last().unwrap();

    let excl = query::gainers_exclusive(&h.store, h.root_id, s1, s2, 10, Extreme::Gainers).unwrap();
    assert!(!excl.is_empty(), "expected a gainer");
    let top = query::full_path(&h.store, excl[0].path_id).unwrap();
    assert!(
        top.ends_with("noisy/deep/c.bin"),
        "exclusive gainer should be the file itself, got {}",
        top.display()
    );
    assert_eq!(excl[0].delta_bytes, 48 << 20);

    // Inclusive rolls the same growth up the ancestor chain, so the root and
    // every ancestor should appear with the identical delta.
    let incl = query::gainers_inclusive(&h.store, h.root_id, s1, s2, 20, Extreme::Gainers).unwrap();
    let paths: Vec<String> = incl
        .iter()
        .map(|g| query::full_path(&h.store, g.path_id).unwrap().display().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("noisy/deep")),
        "inclusive gainers should include the containing directory, got {paths:?}"
    );
    assert!(
        incl.iter().all(|g| g.delta_bytes == 48 << 20),
        "every ancestor of the sole change should show the same delta"
    );
}

/// Sizes in the past must stay correct after the data is deleted in the present.
#[test]
fn history_survives_deletion() {
    let mut h = Harness::new();
    h.write("gone/big.bin", 10 << 20);
    h.snapshot();
    let s_before = *h.scans().last().unwrap();
    let before = query::incl_at(&h.store, h.root_path_id(), s_before).unwrap();
    assert!(before.0 >= 10 << 20);

    fs::remove_dir_all(h.dir.path().join("gone")).unwrap();
    h.snapshot();
    let s_after = *h.scans().last().unwrap();

    // The past is unchanged...
    let still = query::incl_at(&h.store, h.root_path_id(), s_before).unwrap();
    assert_eq!(still, before, "deleting data must not rewrite history");
    // ...and the present reflects the deletion.
    let now = query::incl_at(&h.store, h.root_path_id(), s_after).unwrap();
    assert!(now.0 < before.0, "deletion should be visible in the present");
}

/// Wall-clock instants resolve to the most recent scan at or before them.
#[test]
fn resolve_scan_picks_the_preceding_sample() {
    let mut h = Harness::new();
    h.write("a.bin", 1 << 20);
    h.snapshot();
    let t1 = h.clock;
    h.write("b.bin", 1 << 20);
    h.snapshot();
    let t2 = h.clock;

    let (id_mid, at) = query::resolve_scan(&h.store, h.root_id, t1 + 60).unwrap().unwrap();
    assert_eq!(at, t1, "an instant between samples must resolve backwards, not forwards");
    assert_eq!(id_mid, h.scans()[0]);

    let (id_late, _) = query::resolve_scan(&h.store, h.root_id, t2 + 9999).unwrap().unwrap();
    assert_eq!(id_late, h.scans()[1]);

    assert!(
        query::resolve_scan(&h.store, h.root_id, t1 - 1).unwrap().is_none(),
        "before the first scan there is nothing to report"
    );
}

/// Asking for losers must never return growth.
///
/// Ordering ascending is not sufficient. On a tree with one shrinker and many
/// growers, an ascending top-20 fills the remaining 19 slots with the smallest
/// *gainers* — printing "+120 B" under a heading that says "what shrank".
#[test]
fn losers_never_include_growth() {
    let mut h = Harness::new();
    for i in 0..6 {
        h.write(&format!("g{i}/f.bin"), (2 << 20) + i);
    }
    h.write("shrinker/f.bin", 20 << 20);
    h.snapshot();
    let s1 = *h.scans().last().unwrap();

    // One thing shrinks; everything else grows.
    h.write("shrinker/f.bin", 1 << 20);
    for i in 0..6 {
        h.write(&format!("g{i}/f.bin"), (9 << 20) + i);
    }
    h.snapshot();
    let s2 = *h.scans().last().unwrap();

    let losers =
        query::gainers_exclusive(&h.store, h.root_id, s1, s2, 20, Extreme::Losers).unwrap();
    assert!(!losers.is_empty(), "expected the shrinker to be found");
    assert!(
        losers.iter().all(|g| g.delta_bytes < 0),
        "a losers query returned growth: {:?}",
        losers.iter().map(|g| g.delta_bytes).collect::<Vec<_>>()
    );
    let top = query::full_path(&h.store, losers[0].path_id).unwrap();
    assert!(top.ends_with("shrinker/f.bin"), "wrong loser: {}", top.display());

    // ...and symmetrically, gainers must never include shrinkage.
    let gainers =
        query::gainers_exclusive(&h.store, h.root_id, s1, s2, 20, Extreme::Gainers).unwrap();
    assert!(gainers.iter().all(|g| g.delta_bytes > 0));
}

/// Collapsing ancestors leaves the deepest node that explains the growth.
#[test]
fn collapse_ancestors_keeps_only_the_explaining_node() {
    let mut h = Harness::new();
    h.write("a/b/c/d/blob.bin", 2 << 20);
    h.write("sibling/other.bin", 2 << 20);
    h.snapshot();
    let s1 = *h.scans().last().unwrap();
    h.write("a/b/c/d/blob.bin", 200 << 20);
    h.snapshot();
    let s2 = *h.scans().last().unwrap();

    let raw =
        query::gainers_inclusive(&h.store, h.root_id, s1, s2, 100, Extreme::Gainers).unwrap();
    // Every ancestor reports the same delta, so the raw list is a chain.
    assert!(raw.len() >= 5, "expected the full ancestor chain, got {}", raw.len());

    let collapsed = query::collapse_ancestors(&raw, 0.9);
    assert_eq!(
        collapsed.len(),
        1,
        "one file grew, so exactly one row should survive; got {:?}",
        collapsed
            .iter()
            .map(|g| query::full_path(&h.store, g.path_id).unwrap().display().to_string())
            .collect::<Vec<_>>()
    );
    let kept = query::full_path(&h.store, collapsed[0].path_id).unwrap();
    assert!(kept.ends_with("a/b/c/d/blob.bin"), "kept the wrong node: {}", kept.display());
}

/// A directory whose growth is genuinely spread across children survives
/// collapsing — that directory *is* the insight.
#[test]
fn collapse_keeps_directories_with_several_contributors() {
    let mut h = Harness::new();
    h.write("cache/one.bin", 2 << 20);
    h.write("cache/two.bin", 2 << 20);
    h.write("cache/three.bin", 2 << 20);
    h.snapshot();
    let s1 = *h.scans().last().unwrap();
    // Three siblings each grow by the same amount: no single child dominates.
    h.write("cache/one.bin", 40 << 20);
    h.write("cache/two.bin", 40 << 20);
    h.write("cache/three.bin", 40 << 20);
    h.snapshot();
    let s2 = *h.scans().last().unwrap();

    let raw =
        query::gainers_inclusive(&h.store, h.root_id, s1, s2, 100, Extreme::Gainers).unwrap();
    let collapsed = query::collapse_ancestors(&raw, 0.9);
    let paths: Vec<String> = collapsed
        .iter()
        .map(|g| query::full_path(&h.store, g.path_id).unwrap().display().to_string())
        .collect();
    assert!(
        paths.iter().any(|p| p.ends_with("cache")),
        "a directory with three equal contributors must survive collapsing, got {paths:?}"
    );
}

/// A scan that could not read part of the tree must say so — and must still
/// be usable.
///
/// Both halves matter, and the second is the one that nearly broke. Marking
/// an incomplete scan `partial` is worthless if every query then filters on
/// `status = 'ok'`: a host with one permanently unreadable directory produces
/// nothing but partial scans, and a UI that hides them shows an empty
/// history and no data at all. The totals are an underestimate, but they are
/// internally consistent, and the shortfall is the same directory each time,
/// so the trend still holds. Underreported-but-labelled beats absent.
#[test]
#[cfg(unix)]
fn an_unreadable_directory_makes_the_scan_partial_but_usable() {
    use std::os::unix::fs::PermissionsExt;

    let mut h = Harness::new();
    h.write("open/visible.bin", 4 << 20);
    h.write("closed/hidden.bin", 9 << 20);

    let closed = h.dir.path().join("closed");
    fs::set_permissions(&closed, fs::Permissions::from_mode(0o000)).unwrap();
    // Restore before the TempDir drops, or cleanup fails and the next run
    // inherits the wreckage.
    let restore = scopeguard(closed.clone());

    let (stats, _, _) = h.snapshot();
    let scan_id = stats.scan_id;

    let (status, err): (String, Option<String>) = h
        .store
        .conn
        .query_row("SELECT status, err FROM scan WHERE scan_id = ?1", [scan_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(status, "partial", "an unreadable directory was recorded as a clean scan");
    let err = err.expect("partial scan recorded no reason");
    assert!(err.contains("could not be read"), "{err}");
    // A count is not actionable; the path names the permission to fix.
    assert!(err.contains("closed"), "the failing path was not reported: {err}");

    // The half it could read is still counted, and is not silently zero.
    let total: i64 = h
        .store
        .conn
        .query_row("SELECT incl_bytes FROM scan WHERE scan_id = ?1", [scan_id], |r| r.get(0))
        .unwrap();
    assert!(total >= 4 << 20, "the readable half went missing too: {total}");
    assert!(total < 13 << 20, "the unreadable half was somehow counted: {total}");

    // The part that nearly broke: a partial scan must still be found.
    assert_eq!(
        h.store.last_scan(h.root_id).unwrap(),
        Some(scan_id),
        "a partial scan is invisible to last_scan, so the UI would show nothing"
    );
    assert_eq!(h.store.scan_count(h.root_id).unwrap(), 1);
    assert!(h.store.first_scan(h.root_id).unwrap().is_some());
    assert!(
        query::resolve_scan(&h.store, h.root_id, h.clock + 10_000).unwrap().is_some(),
        "a partial scan cannot be resolved by time, so history queries return nothing"
    );

    drop(restore);
}

/// Put a mode-000 directory back so the temp dir can be removed.
fn scopeguard(p: std::path::PathBuf) -> impl Drop {
    struct G(std::path::PathBuf);
    impl Drop for G {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    G(p)
}
