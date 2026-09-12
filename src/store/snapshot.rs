//! A whole tree as it stood at one instant, materialized in RAM.
//!
//! This is what the API serves treemaps and stacked areas from. Holding the
//! dictionary resident turns subtree aggregation into a pointer walk over
//! contiguous arrays — ~127k entities is about 10 MB as struct-of-arrays,
//! against 500 MB-1 GB for the same tree as individual heap objects.
//!
//! Loading is deliberately split by age. "Now" reads `current_size` directly,
//! one indexed scan. A historical instant has to find each path's latest event
//! at or before that scan, which is the one genuinely expensive query in the
//! design; snapshots are therefore cached by `(root, scan)`.

use super::Store;
use crate::model::{Kind, PathId, RootId, ScanId};
use anyhow::Result;
use rusqlite::params;
use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

pub const NONE: u32 = u32::MAX;

pub struct Snapshot {
    pub root_id: RootId,
    pub scan_id: ScanId,
    pub started_at: i64,

    pub ids: Vec<PathId>,
    pub index: HashMap<PathId, u32>,
    pub parent: Vec<u32>,
    pub depth: Vec<u32>,
    pub name: Vec<OsString>,
    pub kind: Vec<Kind>,
    pub children: Vec<Vec<u32>>,

    pub own_bytes: Vec<i64>,
    pub own_blocks: Vec<i64>,
    pub own_files: Vec<i64>,

    pub incl_bytes: Vec<i64>,
    pub incl_blocks: Vec<i64>,
    pub incl_files: Vec<i64>,
    pub incl_dirs: Vec<i64>,
}

impl Snapshot {
    pub fn load(store: &Store, root_id: RootId, scan_id: ScanId) -> Result<Self> {
        // Every statement below must see the same database.
        //
        // Without a transaction they do not: `load_own` asks whether this is
        // the latest scan and then reads `current_size`, and a commit landing
        // between those two queries yields a tree labelled with one scan but
        // holding the next one's sizes. Narrow, silent, and permanent once
        // cached. A deferred read transaction pins one consistent view; under
        // WAL it blocks nothing.
        let tx = store.conn.unchecked_transaction()?;

        let started_at: i64 = store
            .conn
            .query_row("SELECT started_at FROM scan WHERE scan_id = ?1", params![scan_id], |r| {
                r.get(0)
            })
            .unwrap_or(0);

        // ── dictionary as of this instant ────────────────────────────────
        let mut st = store.conn.prepare(
            "SELECT path_id, parent_id, name, kind, depth FROM path
             WHERE root_id = ?1 AND born_scan <= ?2
               AND (died_scan IS NULL OR died_scan > ?2)
             ORDER BY depth, path_id",
        )?;
        let rows = st.query_map(params![root_id, scan_id], |r| {
            Ok((
                r.get::<_, PathId>(0)?,
                r.get::<_, Option<PathId>>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
            ))
        })?;

        let mut s = Snapshot {
            root_id,
            scan_id,
            started_at,
            ids: Vec::new(),
            index: HashMap::new(),
            parent: Vec::new(),
            depth: Vec::new(),
            name: Vec::new(),
            kind: Vec::new(),
            children: Vec::new(),
            own_bytes: Vec::new(),
            own_blocks: Vec::new(),
            own_files: Vec::new(),
            incl_bytes: Vec::new(),
            incl_blocks: Vec::new(),
            incl_files: Vec::new(),
            incl_dirs: Vec::new(),
        };

        let mut raw_parent: Vec<Option<PathId>> = Vec::new();
        for row in rows {
            let (id, par, name, kind, depth) = row?;
            s.index.insert(id, s.ids.len() as u32);
            s.ids.push(id);
            raw_parent.push(par);
            s.name.push(OsString::from_vec(name));
            s.kind.push(Kind::from_i64(kind).unwrap_or(Kind::Other));
            s.depth.push(depth as u32);
        }
        let n = s.ids.len();
        s.parent = raw_parent
            .iter()
            .map(|p| p.and_then(|id| s.index.get(&id).copied()).unwrap_or(NONE))
            .collect();
        s.children = vec![Vec::new(); n];
        for i in 0..n {
            let p = s.parent[i];
            if p != NONE {
                s.children[p as usize].push(i as u32);
            }
        }

        s.own_bytes = vec![0; n];
        s.own_blocks = vec![0; n];
        s.own_files = vec![0; n];
        s.load_own(store, root_id, scan_id)?;
        s.rollup();
        // Read-only: rolling back is both correct and cheaper than committing.
        drop(tx);
        Ok(s)
    }

    fn load_own(&mut self, store: &Store, root_id: RootId, scan_id: ScanId) -> Result<()> {
        let latest = store.last_scan(root_id)?;
        if latest == Some(scan_id) {
            // The common case: the live view. One indexed scan.
            let mut st = store.conn.prepare(
                "SELECT c.path_id, c.own_bytes, c.own_blocks, c.own_files
                 FROM current_size c JOIN path p ON p.path_id = c.path_id
                 WHERE p.root_id = ?1 AND p.died_scan IS NULL",
            )?;
            let rows = st.query_map(params![root_id], |r| {
                Ok((r.get::<_, PathId>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
            })?;
            for row in rows {
                let (id, b, k, f) = row?;
                if let Some(&i) = self.index.get(&id) {
                    self.own_bytes[i as usize] = b;
                    self.own_blocks[i as usize] = k;
                    self.own_files[i as usize] = f;
                }
            }
            return Ok(());
        }

        // Historical: each path's latest event at or before `scan_id`.
        //
        // This walks all history, which is acceptable today and is the thing
        // to optimize first if snapshot loads ever get slow. The cheap version
        // replays deltas backwards from `current_size`, which needs
        // SUBTREE_GONE rows to carry an exclusive delta alongside the
        // inclusive one they carry now.
        let mut st = store.conn.prepare(
            "SELECT path_id, own_bytes, own_blocks, own_files FROM (
               SELECT e.path_id, e.own_bytes, e.own_blocks, e.own_files,
                      ROW_NUMBER() OVER (PARTITION BY e.path_id ORDER BY e.scan_id DESC) rn
               FROM size_event e
               JOIN path p ON p.path_id = e.path_id
               WHERE p.root_id = ?1 AND e.scan_id <= ?2
             ) WHERE rn = 1",
        )?;
        let rows = st.query_map(params![root_id, scan_id], |r| {
            Ok((r.get::<_, PathId>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?))
        })?;
        for row in rows {
            let (id, b, k, f) = row?;
            if let Some(&i) = self.index.get(&id) {
                self.own_bytes[i as usize] = b;
                self.own_blocks[i as usize] = k;
                self.own_files[i as usize] = f;
            }
        }
        Ok(())
    }

    /// Fold exclusive sizes up into inclusive ones, deepest level first.
    fn rollup(&mut self) {
        let n = self.ids.len();
        self.incl_bytes = self.own_bytes.clone();
        self.incl_blocks = self.own_blocks.clone();
        self.incl_files = self.own_files.clone();
        self.incl_dirs = vec![0; n];
        for i in 0..n {
            match self.kind[i] {
                Kind::Dir => self.incl_dirs[i] = 1,
                Kind::File => self.incl_files[i] += 1,
                _ => {}
            }
        }

        // Rows arrive ordered by depth, so a single reverse pass folds every
        // child into its parent before that parent is itself folded.
        for i in (0..n).rev() {
            let p = self.parent[i];
            if p == NONE {
                continue;
            }
            let p = p as usize;
            self.incl_bytes[p] += self.incl_bytes[i];
            self.incl_blocks[p] += self.incl_blocks[i];
            self.incl_files[p] += self.incl_files[i];
            self.incl_dirs[p] += self.incl_dirs[i];
        }
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn root(&self) -> Option<u32> {
        (0..self.len() as u32).find(|&i| self.parent[i as usize] == NONE)
    }

    pub fn idx(&self, path_id: PathId) -> Option<u32> {
        self.index.get(&path_id).copied()
    }

    /// Resolve a path relative to the root.
    pub fn resolve(&self, rel: &[OsString]) -> Option<u32> {
        let mut cur = self.root()?;
        for want in rel {
            cur = self.children[cur as usize]
                .iter()
                .copied()
                .find(|&c| self.name[c as usize] == *want)?;
        }
        Some(cur)
    }

    /// Full path of a node, as raw OS bytes.
    pub fn path_of(&self, mut i: u32) -> std::path::PathBuf {
        let mut parts = Vec::new();
        loop {
            parts.push(self.name[i as usize].clone());
            let p = self.parent[i as usize];
            if p == NONE {
                break;
            }
            i = p;
        }
        parts.reverse();
        let mut out = std::path::PathBuf::from(&parts[0]);
        for seg in &parts[1..] {
            out.push(seg);
        }
        out
    }
}

impl Snapshot {
    /// Roughly how much heap this snapshot occupies.
    ///
    /// The cache is bounded in bytes rather than in snapshots because the two
    /// are not related: a 100k-entity root costs about 25 MB and a 2.3M-entity
    /// one nearly a gigabyte, so "keep eight" means a comfortable cache on one
    /// machine and an OOM kill on another — and the shipped unit sets
    /// MemoryMax=1G.
    ///
    /// Counted from the actual allocations rather than a per-entity guess:
    /// the variable parts are the names and the per-node child lists, and
    /// those are exactly what differ between a wide tree and a deep one.
    pub fn approx_bytes(&self) -> usize {
        use std::mem::size_of;
        let n = self.ids.len();
        let fixed = n
            * (size_of::<PathId>()          // ids
                + size_of::<u32>() * 2      // parent, depth
                + size_of::<Kind>()
                + size_of::<OsString>()     // the String header; bytes added below
                + size_of::<Vec<u32>>()     // the children header, likewise
                + size_of::<i64>() * 7);    // own_* and incl_*

        // Per-allocation overhead, not just payload. `children` is a vector
        // of vectors, so a 1.3M-entity tree makes 1.3M separate small
        // allocations, each of which costs the allocator a header and rounds
        // up to a size class. Ignoring that undercounted the cache by
        // hundreds of megabytes, which is how a "384 MB" budget produced a
        // resident gigabyte.
        const ALLOC_OVERHEAD: usize = 32;
        let names: usize =
            self.name.iter().map(|s| if s.is_empty() { 0 } else { s.len() + ALLOC_OVERHEAD }).sum();
        let kids: usize = self
            .children
            .iter()
            .map(|c| {
                if c.is_empty() { 0 } else { c.capacity() * size_of::<u32>() + ALLOC_OVERHEAD }
            })
            .sum();
        // A HashMap keeps roughly 1/0.875 slots per entry, each an (id, idx).
        let index = (n * (size_of::<PathId>() + size_of::<u32>()) * 8) / 7;
        fixed + names + kids + index
    }
}
