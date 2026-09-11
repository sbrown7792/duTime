//! Turning a walk into change-only events.
//!
//! This is where duTime earns its storage budget. A scan of this machine's home
//! directory produces ~127k tracked entities; writing all of them every time
//! would be ~5 MB of rows and ~1.4 GB of WAL per day at 5-minute intervals.
//! Instead we reconcile the fresh walk against the stored dictionary and write
//! only what actually moved — measured at ~55 rows per scan.
//!
//! The absence of a row for a scan therefore means "unchanged since this path's
//! previous row". Every reader must honour that carry-forward rule.

use super::{LiveDict, Store};
use crate::model::tree::{NodeIdx, Rollup, Tree};
use crate::model::{PathId, RootId, ScanId, flags};
use crate::scan::walker::ScanStats;
use anyhow::Result;
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct CommitOptions {
    /// Write inclusive keyframes every N scans. Bounds how far an "as of T"
    /// query has to replay.
    pub checkpoint_every_scans: i64,
    /// Only checkpoint entities at least this large; the long tail of tiny
    /// directories is cheap to replay and not worth the rows.
    pub checkpoint_min_bytes: i64,
}

impl Default for CommitOptions {
    fn default() -> Self {
        Self { checkpoint_every_scans: 288, checkpoint_min_bytes: 1 << 20 }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CommitStats {
    pub scan_id: ScanId,
    pub n_events: i64,
    pub n_born: i64,
    pub n_gone: i64,
    pub n_subtree_gone: i64,
    pub n_checkpoints: i64,
    /// Paths whose recorded kind changed (a file replaced by a directory, say).
    pub n_reincarnated: i64,
}

/// Persist one completed walk.
pub fn commit_scan(
    store: &mut Store,
    root_id: RootId,
    root_path: &Path,
    tree: &Tree,
    roll: &Rollup,
    stats: &ScanStats,
    started_at: i64,
    duration_ms: i64,
    opts: &CommitOptions,
) -> Result<CommitStats> {
    let prev_scans = store.scan_count(root_id)?;
    let dict = LiveDict::load(store, root_id)?;
    let vfs = statvfs(root_path);

    let tx = store.conn.transaction()?;
    let mut out = CommitStats::default();

    tx.execute(
        "INSERT INTO scan (root_id, started_at, ended_at, duration_ms, status,
                           n_dirs, n_files, n_entities, incl_bytes, incl_blocks,
                           fs_total, fs_free, fs_avail)
         VALUES (?1, ?2, ?3, ?4, 'ok', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            root_id,
            started_at,
            started_at + duration_ms / 1000,
            duration_ms,
            stats.n_dirs,
            stats.n_files,
            tree.len() as i64,
            roll.bytes.first().copied().unwrap_or(0),
            roll.blocks.first().copied().unwrap_or(0),
            vfs.map(|v| v.0),
            vfs.map(|v| v.1),
            vfs.map(|v| v.2),
        ],
    )?;
    let scan_id: ScanId = tx.last_insert_rowid();
    out.scan_id = scan_id;

    // ── 1. Reconcile the dictionary ──────────────────────────────────────
    //
    // Node indices are in creation order and `ensure_path` always creates a
    // parent before its children, so a single forward pass guarantees a
    // parent's `path_id` is known before any child needs it.
    let mut node_path_id: Vec<PathId> = vec![0; tree.len()];
    let mut matched: HashSet<PathId> = HashSet::with_capacity(dict.len());

    {
        let mut ins_path = tx.prepare(
            "INSERT INTO path (parent_id, name, root_id, depth, kind, born_scan, ino, dev)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, NULL)",
        )?;
        let mut kill_one = tx.prepare("UPDATE path SET died_scan = ?2 WHERE path_id = ?1")?;

        for i in 0..tree.len() {
            let idx = i as NodeIdx;
            let kind = tree.kind[i];
            // The root row carries the full root path as its name, so it is
            // self-describing and cannot collide with another root's row.
            let (parent_db, name) = if i == 0 {
                (None, root_path.as_os_str().to_os_string())
            } else {
                (Some(node_path_id[tree.parent[i] as usize]), tree.name[i].clone())
            };
            let key = (parent_db.unwrap_or(-1), name.clone());

            let existing = dict.by_parent_name.get(&key).copied().filter(|id| {
                // A path whose kind changed is not the same thing as before:
                // its size history is not comparable, so retire it and start a
                // new incarnation rather than splicing two unrelated series.
                dict.by_id.get(id).map(|lp| lp.kind == kind).unwrap_or(false)
            });

            let path_id = match existing {
                Some(id) => {
                    matched.insert(id);
                    id
                }
                None => {
                    if let Some(&stale) = dict.by_parent_name.get(&key) {
                        kill_one.execute(params![stale, scan_id])?;
                        out.n_reincarnated += 1;
                    }
                    ins_path.execute(params![
                        parent_db,
                        name.as_bytes(),
                        root_id,
                        tree.depth[i] as i64,
                        kind as i64,
                        scan_id,
                    ])?;
                    let id = tx.last_insert_rowid();
                    out.n_born += 1;
                    id
                }
            };
            node_path_id[idx as usize] = path_id;
        }
    }

    // ── 2. Deletions ─────────────────────────────────────────────────────
    //
    // A mass delete must not explode into a row per descendant: removing a
    // node_modules/ with 30k directories has to cost one event, not 30,000.
    // We emit a single SUBTREE_GONE at the top of each deleted subtree
    // carrying the whole subtree's inclusive loss, and mark the descendants
    // dead with dictionary updates only.
    let deleted: HashSet<PathId> =
        dict.by_id.keys().copied().filter(|id| !matched.contains(id)).collect();

    let mut event_rows: Vec<(PathId, i64, i64, i64, i64, i64, i64)> = Vec::new();

    if !deleted.is_empty() {
        let mut kill = tx.prepare("UPDATE path SET died_scan = ?2 WHERE path_id = ?1")?;
        let mut drop_cur = tx.prepare("DELETE FROM current_size WHERE path_id = ?1")?;

        for &id in &deleted {
            let parent = dict.by_id[&id].parent_id.unwrap_or(-1);
            // Only the topmost deleted node in each subtree emits an event.
            if parent != -1 && deleted.contains(&parent) {
                kill.execute(params![id, scan_id])?;
                drop_cur.execute(params![id])?;
                continue;
            }

            let sub = dict.subtree(id);
            let mut lost_bytes = 0i64;
            let mut lost_blocks = 0i64;
            for s in &sub {
                if let Some(lp) = dict.by_id.get(s) {
                    lost_bytes += lp.own_bytes;
                    lost_blocks += lp.own_blocks;
                }
            }
            let flag = if sub.len() > 1 { flags::SUBTREE_GONE } else { flags::GONE };
            if sub.len() > 1 {
                out.n_subtree_gone += 1;
            } else {
                out.n_gone += 1;
            }
            event_rows.push((id, 0, 0, 0, -lost_bytes, -lost_blocks, flag));

            for s in sub {
                kill.execute(params![s, scan_id])?;
                drop_cur.execute(params![s])?;
            }
        }
    }

    // ── 3. Size changes ──────────────────────────────────────────────────
    //
    // A file crossing the tracking threshold moves bytes out of its parent's
    // exclusive size and into a new entity in the same scan. Because both
    // sides are recomputed from the fresh walk, the two deltas cancel exactly
    // and no ancestor's history drifts. That cancellation is asserted in
    // `tests/store_invariants.rs`.
    for i in 0..tree.len() {
        let path_id = node_path_id[i];
        let (nb, nk, nf) = (tree.own_bytes[i], tree.own_blocks[i], tree.own_files[i]);
        let prev = dict.by_id.get(&path_id);
        let (ob, ok, of) = prev.map(|p| (p.own_bytes, p.own_blocks, p.own_files)).unwrap_or((0, 0, 0));

        if prev.is_some() && nb == ob && nk == ok && nf == of {
            continue; // unchanged: carry-forward covers it
        }
        let flag = if prev.is_none() { flags::BORN } else { 0 };
        event_rows.push((path_id, nb, nk, nf, nb - ob, nk - ok, flag));
    }

    // ── 4. Write ─────────────────────────────────────────────────────────
    {
        let mut ins_ev = tx.prepare(
            "INSERT INTO size_event
               (path_id, scan_id, own_bytes, own_blocks, own_files, d_bytes, d_blocks, flags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut upsert_cur = tx.prepare(
            "INSERT INTO current_size (path_id, own_bytes, own_blocks, own_files)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(path_id) DO UPDATE SET
               own_bytes = excluded.own_bytes,
               own_blocks = excluded.own_blocks,
               own_files = excluded.own_files",
        )?;
        for &(path_id, ob, okb, of, db, dkb, fl) in &event_rows {
            ins_ev.execute(params![path_id, scan_id, ob, okb, of, db, dkb, fl])?;
            if fl & (flags::GONE | flags::SUBTREE_GONE) == 0 {
                upsert_cur.execute(params![path_id, ob, okb, of])?;
            }
        }
        out.n_events = event_rows.len() as i64;
    }

    // ── 5. Checkpoints ───────────────────────────────────────────────────
    //
    // Always on the very first scan, so the slow cold reconstruction path
    // (last-value-per-path over all history) never has to run in production.
    let due = prev_scans == 0 || (prev_scans + 1) % opts.checkpoint_every_scans == 0;
    if due {
        let mut ins_cp = tx.prepare(
            "INSERT OR REPLACE INTO checkpoint
               (path_id, scan_id, incl_bytes, incl_blocks, incl_files, incl_dirs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for i in 0..tree.len() {
            if i != 0 && roll.bytes[i] < opts.checkpoint_min_bytes {
                continue;
            }
            ins_cp.execute(params![
                node_path_id[i],
                scan_id,
                roll.bytes[i],
                roll.blocks[i],
                roll.files[i],
                roll.dirs[i],
            ])?;
            out.n_checkpoints += 1;
        }
    }

    tx.execute("UPDATE scan SET n_events = ?2 WHERE scan_id = ?1", params![scan_id, out.n_events])?;
    tx.commit()?;
    Ok(out)
}

/// `(total, free, available)` bytes for the filesystem holding `path`.
///
/// Feeds the "unaccounted space" reconciliation: the gap between what the
/// filesystem says is used and what duTime can attribute to a path is where
/// deleted-but-still-open files and mis-scoped excludes show up.
fn statvfs(path: &Path) -> Option<(i64, i64, i64)> {
    let v = rustix::fs::statvfs(path).ok()?;
    let f = v.f_frsize as i64;
    Some((v.f_blocks as i64 * f, v.f_bfree as i64 * f, v.f_bavail as i64 * f))
}

/// Map every tracked entity to its database id, for callers that need to join
/// a fresh walk against stored history.
pub fn path_ids_by_rel(tree: &Tree, node_path_id: &[PathId]) -> HashMap<Vec<std::ffi::OsString>, PathId> {
    (0..tree.len())
        .map(|i| (tree.rel_path(i as NodeIdx), node_path_id[i]))
        .collect()
}

/// Debug helper: does `current_size` agree with a full replay of `size_event`?
///
/// `current_size` is a materialization, so it can in principle drift from the
/// events it summarizes. It is written in the same transaction, but a bug in
/// the commit path would be invisible without this check — and it would corrupt
/// every future delta silently.
pub fn verify_current_size(store: &Store, root_id: RootId) -> Result<Vec<PathId>> {
    let mut st = store.conn.prepare(
        "WITH latest AS (
           SELECT e.path_id, e.own_bytes, e.own_blocks, e.own_files,
                  ROW_NUMBER() OVER (PARTITION BY e.path_id ORDER BY e.scan_id DESC) rn
           FROM size_event e
           JOIN path p ON p.path_id = e.path_id
           WHERE p.root_id = ?1 AND p.died_scan IS NULL
         )
         SELECT l.path_id FROM latest l
         LEFT JOIN current_size c ON c.path_id = l.path_id
         WHERE l.rn = 1
           AND (c.own_bytes IS NOT l.own_bytes
             OR c.own_blocks IS NOT l.own_blocks
             OR c.own_files IS NOT l.own_files)",
    )?;
    let rows = st.query_map(params![root_id], |r| r.get::<_, PathId>(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

