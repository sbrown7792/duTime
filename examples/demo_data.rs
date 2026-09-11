//! Generate a demo database with realistic history.
//!
//! Development tool. Builds a small tree, then simulates a fortnight of hourly
//! scans against it — steady log growth, a cache that fills and gets cleared, a
//! project that balloons, and a one-off big download that later gets deleted.
//! Without a few hundred backdated samples there is nothing for the time
//! slider, the stacked area or the diff treemap to actually show.
//!
//! Usage: cargo run --release --example demo_data -- /tmp/dutime-demo.db

use dutime::scan::walker::{ScanOptions, scan};
use dutime::store::Store;
use dutime::store::commit::{CommitOptions, commit_scan};
use std::fs;
use std::io::Write;
use std::path::Path;

const HOURS: i64 = 24 * 14;
const TREE: &str = "/tmp/dutime-demo-tree";

fn write(p: &Path, n: usize) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    let mut f = fs::File::create(p).unwrap();
    // Write in chunks so a 200 MB file doesn't need 200 MB of RAM.
    let chunk = vec![b'x'; 1 << 16];
    let mut left = n;
    while left > 0 {
        let k = left.min(chunk.len());
        f.write_all(&chunk[..k]).unwrap();
        left -= k;
    }
}

fn main() -> anyhow::Result<()> {
    let db = std::env::args().nth(1).unwrap_or_else(|| "/tmp/dutime-demo.db".into());
    let _ = fs::remove_file(&db);
    let _ = fs::remove_file(format!("{db}-wal"));
    let _ = fs::remove_file(format!("{db}-shm"));
    let _ = fs::remove_dir_all(TREE);

    let root = Path::new(TREE);
    fs::create_dir_all(root)?;

    // A plausible server layout.
    write(&root.join("srv/app/bin/server"), 18 << 20);
    write(&root.join("srv/app/static/bundle.js"), 3 << 20);
    write(&root.join("srv/data/main.db"), 40 << 20);
    write(&root.join("home/alice/Documents/report.pdf"), 6 << 20);
    write(&root.join("home/alice/Pictures/trip.raw"), 22 << 20);
    write(&root.join("home/bob/notes.md"), 4096);
    for i in 0..8 {
        write(&root.join(format!("var/cache/pkg/blob{i}.bin")), (2 << 20) + i * 1024);
    }

    let mut store = Store::open(&db)?;
    let canon = root.canonicalize()?;
    let root_id = store.ensure_root(&canon)?;
    let opts = CommitOptions { checkpoint_every_scans: 24, checkpoint_min_bytes: 1 << 20 };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let start = now - HOURS * 3600;

    for h in 0..=HOURS {
        let t = start + h * 3600;

        // Logs grow steadily, with a daily rotation that truncates them.
        let day = h / 24;
        let hour_of_day = h % 24;
        write(&root.join("var/log/app.log"), (2 << 20) + (hour_of_day as usize) * (900 << 10));

        // The cache creeps up all week and is cleared on day 9.
        if day == 9 && hour_of_day == 3 {
            let _ = fs::remove_dir_all(root.join("var/cache/pkg"));
        } else if day != 9 || hour_of_day > 3 {
            let n = 8 + (h / 6) as usize;
            for i in 0..n.min(60) {
                let p = root.join(format!("var/cache/pkg/blob{i}.bin"));
                if !p.exists() {
                    write(&p, (2 << 20) + i * 4096);
                }
            }
        }

        // A build directory that balloons from day 4 onward.
        if day >= 4 {
            let grown = ((day - 3) as usize) * (28 << 20);
            write(&root.join("srv/app/target/release/artifact.bin"), grown);
        }

        // A large download that shows up on day 6 and is deleted on day 11 —
        // the case where "what happened to my disk?" has an answer that is
        // invisible to a point-in-time du run today.
        let iso = root.join("home/alice/Downloads/distro.iso");
        if day == 6 && hour_of_day == 14 {
            write(&iso, 700 << 20);
        }
        if day == 11 && hour_of_day == 9 && iso.exists() {
            fs::remove_file(&iso)?;
        }

        // Photos trickle in.
        if hour_of_day == 20 {
            write(&root.join(format!("home/alice/Pictures/day{day}.jpg")), 5 << 20);
        }

        let mut o = ScanOptions::new(&canon);
        o.threads = 2;
        let r = scan(&o)?;
        let roll = r.tree.rollup();
        commit_scan(&mut store, root_id, &canon, &r.tree, &roll, &r.stats, t, 40, &opts)?;

        if h % 48 == 0 {
            println!("  day {:>2}  {:>12}  ({} entities)", day, roll.bytes[0], r.tree.len());
        }
    }

    println!("\nwrote {db} — {HOURS} hourly samples over 14 days");
    println!("tree lives at {TREE}");
    Ok(())
}
