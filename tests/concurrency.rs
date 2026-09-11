//! Reads must not queue behind the scanner.
//!
//! A scan on a large server takes minutes to walk and seconds to commit. If
//! the web UI blocks for that window it is useless exactly when someone is
//! watching a disk fill. Measured before this was fixed: a single shared
//! connection behind one mutex gave a 680 ms worst-case on the `/tree`
//! endpoint during the baseline commit, versus 164 ms after.

use dutime::api::AppState;
use dutime::scan::walker::{ScanOptions, scan};
use dutime::store::commit::{CommitOptions, commit_scan};
use dutime::store::{Store, query};
use std::fs;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
    fs::create_dir_all("target/fixtures").unwrap();
    let dir = tempfile::Builder::new()
        .prefix("dutime-conc-")
        .tempdir_in("target/fixtures")
        .unwrap();
    let root = dir.path().canonicalize().unwrap();
    for i in 0..400 {
        let p = root.join(format!("d{}/f{i}.bin", i % 20));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::File::create(&p).unwrap().write_all(&vec![b'x'; (1 << 20) + i]).unwrap();
    }
    (dir, root)
}

/// A reader can query while a write transaction is open on the writer.
///
/// This is the whole point of WAL plus separate connections. With one shared
/// connection the read would block until the commit finished.
#[test]
fn reads_proceed_while_a_write_transaction_is_open() {
    let db = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
    let path = db.path().to_path_buf();
    drop(db);

    let (_dir, root) = fixture();
    let state = Arc::new(AppState::open(&path).unwrap());

    // Seed one scan so there is something to read.
    {
        let mut store = state.store.lock().unwrap();
        let root_id = store.ensure_root(&root).unwrap();
        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        commit_scan(
            &mut store, root_id, &root, &r.tree, &roll, &r.stats,
            dutime::cli::now(), 10, &CommitOptions::default(),
        )
        .unwrap();
    }

    let root_id = state.read().roots().unwrap()[0].0;

    // Hold an open write transaction, as a long commit would.
    let writer = state.store.lock().unwrap();
    let tx = writer.conn.unchecked_transaction().unwrap();
    tx.execute(
        "INSERT INTO annotation (at, kind, label) VALUES (?1, 'test', 'blocking')",
        [dutime::cli::now()],
    )
    .unwrap();

    // ...and read anyway, from the pool, with a deadline well under the
    // 10-second busy_timeout so a blocked read fails rather than hangs.
    let t0 = Instant::now();
    let got = {
        let r = state.read();
        let last = r.last_scan(root_id).unwrap().unwrap();
        let pid: i64 = r
            .conn
            .query_row(
                "SELECT path_id FROM path WHERE root_id = ?1 AND parent_id IS NULL",
                [root_id],
                |row| row.get(0),
            )
            .unwrap();
        query::incl_at(&r, pid, last).unwrap()
    };
    let elapsed = t0.elapsed();

    drop(tx);
    drop(writer);

    assert!(got.0 > 0, "the read returned nothing");
    assert!(
        elapsed < Duration::from_millis(500),
        "a read took {elapsed:?} while a write transaction was open — it is \
         queueing behind the writer instead of using its own connection"
    );
}

/// Several readers can work at once.
#[test]
fn readers_run_concurrently() {
    let db = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
    let path = db.path().to_path_buf();
    drop(db);

    let (_dir, root) = fixture();
    let state = Arc::new(AppState::open(&path).unwrap());
    {
        let mut store = state.store.lock().unwrap();
        let root_id = store.ensure_root(&root).unwrap();
        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        commit_scan(
            &mut store, root_id, &root, &r.tree, &roll, &r.stats,
            dutime::cli::now(), 10, &CommitOptions::default(),
        )
        .unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for _ in 0..4 {
        let (state, stop, done) = (state.clone(), stop.clone(), done.clone());
        hs.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let r = state.read();
                let _ = r.roots().unwrap();
                done.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    for h in hs {
        h.join().unwrap();
    }
    assert!(
        done.load(Ordering::Relaxed) > 100,
        "four reader threads managed only {} queries in 300 ms; the pool is \
         serializing them",
        done.load(Ordering::Relaxed)
    );
}

/// Snapshots are immutable once their scan is committed, so the cache must
/// survive later scans.
///
/// Clearing the whole cache on every scan made the time slider reload every
/// historical snapshot from scratch each time a new sample landed.
#[test]
fn cached_snapshots_survive_later_scans() {
    let db = tempfile::Builder::new().suffix(".db").tempfile().unwrap();
    let path = db.path().to_path_buf();
    drop(db);

    let (_dir, root) = fixture();
    let state = Arc::new(AppState::open(&path).unwrap());
    let root_id;
    let first_scan;
    {
        let mut store = state.store.lock().unwrap();
        root_id = store.ensure_root(&root).unwrap();
        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        first_scan = commit_scan(
            &mut store, root_id, &root, &r.tree, &roll, &r.stats,
            dutime::cli::now() - 60, 10, &CommitOptions::default(),
        )
        .unwrap()
        .scan_id;
    }

    let before = state.snapshot(root_id, first_scan).unwrap();
    let total_before = before.root().map(|i| before.incl_bytes[i as usize]).unwrap();

    // Add data and scan again.
    fs::File::create(root.join("d0/new.bin"))
        .unwrap()
        .write_all(&vec![b'y'; 8 << 20])
        .unwrap();
    {
        let mut store = state.store.lock().unwrap();
        let mut o = ScanOptions::new(&root);
        o.threads = 2;
        let r = scan(&o).unwrap();
        let roll = r.tree.rollup();
        commit_scan(
            &mut store, root_id, &root, &r.tree, &roll, &r.stats,
            dutime::cli::now(), 10, &CommitOptions::default(),
        )
        .unwrap();
    }

    // The old snapshot must still describe the old state, whether it came
    // from the cache or was rebuilt.
    let after = state.snapshot(root_id, first_scan).unwrap();
    let total_after = after.root().map(|i| after.incl_bytes[i as usize]).unwrap();
    assert_eq!(
        total_before, total_after,
        "a snapshot of a past scan changed after a later scan landed"
    );
    assert!(Arc::ptr_eq(&before, &after), "the cached snapshot was needlessly discarded");
}
