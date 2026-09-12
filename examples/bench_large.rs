//! Build a database the size of a real Nextcloud volume and time the API.
//!
//! 2.3M entities cannot be measured by creating 2.3M files — the fixture
//! would take longer to build than the thing under test takes to run, and
//! would be measuring the filesystem rather than duTime. The tree is
//! synthesised in memory and committed through the ordinary commit path
//! instead, so the rows on disk are exactly the rows a real scan writes.

use dutime::model::Kind;
use dutime::model::tree::Tree;
use dutime::store::Store;
use dutime::store::commit::{CommitOptions, commit_scan};
use dutime::scan::walker::ScanStats;
use std::ffi::OsString;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db = args.get(1).cloned().unwrap_or_else(|| "/tmp/dutime-large.db".into());
    let dirs: usize = args.get(2).map(|s| s.parse().unwrap()).unwrap_or(1_300_000);
    let scans: usize = args.get(3).map(|s| s.parse().unwrap()).unwrap_or(6);

    let _ = std::fs::remove_file(&db);
    let _ = std::fs::remove_file(format!("{db}-wal"));
    let _ = std::fs::remove_file(format!("{db}-shm"));
    let mut store = Store::open(&db)?;
    let root_path = std::path::Path::new("/media/nextcloud");
    let root_id = store.ensure_root(root_path)?;

    let mut clock = 1_780_000_000i64;
    for s in 0..scans {
        let t0 = Instant::now();
        let tree = build(dirs, s);
        let built = t0.elapsed();
        let roll = tree.rollup();
        let stats = ScanStats {
            n_dirs: dirs as i64,
            n_files: (dirs as i64) * 3 / 4,
            n_entities: tree.len() as i64,
            ..Default::default()
        };
        clock += 3600;
        let t1 = Instant::now();
        let c = commit_scan(
            &mut store, root_id, root_path, &tree, &roll, &stats, clock, 900_000,
            &CommitOptions { checkpoint_every_scans: 24, checkpoint_min_bytes: 1 << 20 },
        )?;
        println!(
            "scan {:<2} {:>9} entities  build {:>6.2}s  commit {:>6.2}s  {:>8} events",
            c.scan_id,
            tree.len(),
            built.as_secs_f64(),
            t1.elapsed().as_secs_f64(),
            c.n_events
        );
    }

    let bytes = std::fs::metadata(&db).map(|m| m.len()).unwrap_or(0);
    println!("\ndatabase {} MiB at {db}", bytes >> 20);
    Ok(())
}

/// A tree shaped like user data: a wide fan of accounts, each with a deep-ish
/// nest of folders. `round` shifts the sizes so later scans produce real churn.
fn build(dirs: usize, round: usize) -> Tree {
    let mut t = Tree::new();
    let root = t.add_root(OsString::from("/media/nextcloud"), Kind::Dir);
    let data = t.get_or_insert(root, &OsString::from("data"), Kind::Dir);

    let users = 40usize;
    let per_user = dirs / users;
    let mut made = 0usize;

    for u in 0..users {
        let ud = t.get_or_insert(data, &OsString::from(format!("user{u:03}")), Kind::Dir);
        let files_dir = t.get_or_insert(ud, &OsString::from("files"), Kind::Dir);
        // ~200 folders per album, ~depth 5, which matches the measured mean
        // directory depth of a real tree closely enough for a timing test.
        let albums = per_user / 200;
        for a in 0..albums {
            let al = t.get_or_insert(files_dir, &OsString::from(format!("album{a:05}")), Kind::Dir);
            for d in 0..200 {
                if made >= dirs {
                    break;
                }
                let leaf = t.get_or_insert(al, &OsString::from(format!("d{d:03}")), Kind::Dir);
                let i = leaf as usize;
                // Only a slice of the tree changes between scans, the way a
                // real volume behaves — otherwise every scan is a full
                // rewrite and the event counts are meaningless.
                let churn = if (i + round).is_multiple_of(5000) { round as i64 * 4096 } else { 0 };
                t.own_bytes[i] = 4096 + (i as i64 % 7) * 1024 + churn;
                t.own_blocks[i] = ((t.own_bytes[i] + 511) / 512) * 512;
                t.own_files[i] = 3;
                made += 1;
            }
        }
    }
    t
}
