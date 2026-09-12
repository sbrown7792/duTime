//! Shared server state: the writer, a pool of readers, and the snapshot cache.
//!
//! The scanner and the web UI must not wait on each other. A scan on a large
//! server can take minutes to walk and seconds to commit, and a dashboard that
//! freezes for the duration is a dashboard nobody trusts.
//!
//! Two things make that work:
//!
//! * **WAL plus separate connections.** SQLite allows any number of concurrent
//!   readers alongside the single writer — but only across distinct
//!   connections. One `Mutex<Connection>` shared by everything throws that
//!   guarantee away and serializes every request behind the commit.
//! * **Reads happen on the blocking pool.** SQLite calls are synchronous; run
//!   them on async worker threads and a handful of slow queries starve the
//!   runtime, at which point even the health check stops answering.

use crate::model::{RootId, ScanId};
use crate::store::Store;
use crate::store::snapshot::Snapshot;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

/// How many materialized snapshots to keep, and how much memory they may
/// take between them.
///
/// The byte budget is the real bound; the count is a secondary cap so a
/// pathologically small root cannot fill the cache with hundreds of entries.
/// Counting alone is not enough — a snapshot of a 127k-entity tree is about
/// 25 MB and one of a 2.3M-entity volume approaches a gigabyte, so "keep
/// eight" is a modest cache on one machine and an OOM kill on another. The
/// shipped unit sets MemoryMax=1G, and the walker needs room of its own.
const MAX_CACHED_SNAPSHOTS: usize = 8;
const SNAPSHOT_CACHE_BYTES: usize = 192 << 20;

/// Read connections. Enough for the handful of parallel requests a dashboard
/// makes, few enough that a burst cannot exhaust the blocking pool.
const READERS: usize = 4;

pub struct AppState {
    /// The scanner's connection. Held only for the duration of a commit.
    pub store: Mutex<Store>,
    readers: ReadPool,
    cache: Mutex<Cache>,
    pub auth: crate::auth::Auth,
    /// Roots that need the token. Held here rather than in the database
    /// because it is an access policy for the network API, not a property of
    /// the recorded data — the local CLI can already read the filesystem.
    protected: std::collections::HashSet<RootId>,
}

/// A tiny checkout pool. Not worth a dependency: this is the whole thing.
struct ReadPool {
    idle: Mutex<Vec<Store>>,
    available: Condvar,
}

/// A reader borrowed from the pool, returned on drop.
pub struct Reader<'a> {
    pool: &'a ReadPool,
    conn: Option<Store>,
}

impl std::ops::Deref for Reader<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        self.conn.as_ref().expect("reader used after return")
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            self.pool.idle.lock().unwrap().push(c);
            self.pool.available.notify_one();
        }
    }
}

impl ReadPool {
    fn get(&self) -> Reader<'_> {
        let mut idle = self.idle.lock().unwrap();
        // Only ever called from the blocking pool, so parking here costs an
        // OS thread rather than an async worker.
        while idle.is_empty() {
            idle = self.available.wait(idle).unwrap();
        }
        let conn = idle.pop();
        drop(idle);
        Reader { pool: self, conn }
    }
}

#[derive(Default)]
struct Cache {
    map: HashMap<(RootId, ScanId), Arc<Snapshot>>,
    /// Insertion order, oldest first.
    order: Vec<(RootId, ScanId)>,
    bytes: usize,
}

impl AppState {
    /// In-memory state, for tests. Reads and writes share the one connection,
    /// which is fine when there is no scanner running against it.
    pub fn new(store: Store) -> Self {
        Self {
            store: Mutex::new(store),
            readers: ReadPool { idle: Mutex::new(Vec::new()), available: Condvar::new() },
            cache: Mutex::new(Cache::default()),
            auth: crate::auth::Auth::Open,
            protected: std::collections::HashSet::new(),
        }
    }

    /// Open a writer plus a pool of readers against a database on disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path: PathBuf = path.as_ref().to_path_buf();
        // The writer opens first so the WAL and its `-shm` index exist before
        // any reader attaches.
        let writer = Store::open(&path)?;
        let mut idle = Vec::with_capacity(READERS);
        for _ in 0..READERS {
            idle.push(Store::open_reader(&path)?);
        }
        Ok(Self {
            store: Mutex::new(writer),
            readers: ReadPool { idle: Mutex::new(idle), available: Condvar::new() },
            cache: Mutex::new(Cache::default()),
            auth: crate::auth::Auth::Open,
            protected: std::collections::HashSet::new(),
        })
    }

    /// Borrow a connection for reading.
    ///
    /// Falls back to the writer when no pool exists (the in-memory test case),
    /// where there is no scanner to contend with.
    pub fn read(&self) -> ReadGuard<'_> {
        if self.readers.idle.lock().unwrap().is_empty()
            && self.readers.idle.lock().unwrap().capacity() == 0
        {
            return ReadGuard::Writer(self.store.lock().unwrap());
        }
        ReadGuard::Pooled(self.readers.get())
    }

    /// A materialized tree for an instant, cached.
    pub fn snapshot(&self, root: RootId, scan: ScanId) -> Result<Arc<Snapshot>> {
        if let Some(s) = self.cache.lock().unwrap().map.get(&(root, scan)) {
            return Ok(s.clone());
        }
        // Built outside the cache lock: a cold load takes long enough that
        // holding it would serialize every other request behind this one.
        let built = {
            let r = self.read();
            Arc::new(Snapshot::load(&r, root, scan)?)
        };

        let size = built.approx_bytes();
        let mut c = self.cache.lock().unwrap();
        if c.map.insert((root, scan), built.clone()).is_none() {
            c.order.push((root, scan));
            c.bytes += size;
            // Always keep the one just built, however large: evicting it
            // would mean rebuilding it for the very next request, and on a
            // tree this big that is seconds, not milliseconds.
            while c.order.len() > 1
                && (c.order.len() > MAX_CACHED_SNAPSHOTS || c.bytes > SNAPSHOT_CACHE_BYTES)
            {
                let victim = c.order.remove(0);
                if let Some(old) = c.map.remove(&victim) {
                    c.bytes = c.bytes.saturating_sub(old.approx_bytes());
                }
            }
        }
        Ok(built)
    }
}

/// Either a pooled reader or the writer, depending on how state was built.
pub enum ReadGuard<'a> {
    Pooled(Reader<'a>),
    Writer(std::sync::MutexGuard<'a, Store>),
}

impl std::ops::Deref for ReadGuard<'_> {
    type Target = Store;
    fn deref(&self) -> &Store {
        match self {
            ReadGuard::Pooled(r) => r,
            ReadGuard::Writer(w) => w,
        }
    }
}

impl AppState {
    /// Declare which roots need the token, and how to check it.
    ///
    /// Applied after the roots exist in the database, since the policy is
    /// keyed by `root_id` and those are assigned on first sight of a path.
    pub fn set_access(
        &mut self,
        auth: crate::auth::Auth,
        protected: std::collections::HashSet<RootId>,
    ) {
        self.auth = auth;
        self.protected = protected;
    }

    pub fn is_protected(&self, root: RootId) -> bool {
        self.protected.contains(&root)
    }

    pub fn has_protected_roots(&self) -> bool {
        !self.protected.is_empty()
    }
}

impl AppState {
    /// Snapshot cache occupancy, for diagnostics and for the tests that keep
    /// the budget honest.
    pub fn cache_stats(&self) -> serde_json::Value {
        let c = self.cache.lock().unwrap();
        serde_json::json!({
            "snapshots": c.order.len(),
            "bytes": c.bytes,
            "budget_bytes": SNAPSHOT_CACHE_BYTES,
            "max_snapshots": MAX_CACHED_SNAPSHOTS,
        })
    }
}
